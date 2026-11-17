#!/usr/bin/env bash
# Build Pyo's images with the in-cluster BuildKit and (re)deploy Pyo into
# k3s. Run as your normal user. On a machine without k3s it offers to run
# scripts/setup-host.sh first:
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
# Pyo's built-in sandbox templates (base and desktop) take the images
# upsetdev/pyo-sandbox-images publishes, which Pyo finds itself.
# PYO_UP_SANDBOX_IMAGES=local builds them instead, from a checkout of that
# repo beside this one (../pyo-sandbox-images), into the local registry
# (desktop on that base), and pins Pyo to them. That's about 8 GB of
# layers, so it's only done when asked:
#
#   PYO_UP_SANDBOX_IMAGES=local scripts/pyo-up.sh
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

TTY="" BOLD="" RED="" GREEN="" CYAN="" DIM="" RESET=""
if [ -t 1 ] && [ "${TERM:-dumb}" != dumb ]; then
  TTY=1
  [ -n "${NO_COLOR:-}" ] || BOLD=$'\033[1m' RED=$'\033[31m' GREEN=$'\033[32m' CYAN=$'\033[36m' DIM=$'\033[2m' RESET=$'\033[0m'
fi
log() { printf '%s→ %s%s\n' "$BOLD" "$*" "$RESET"; }

# The sandbox templates' images: published (the default) or built here.
SANDBOX_SRC=""
case "${PYO_UP_SANDBOX_IMAGES:-published}" in
  published) ;;
  local)
    SANDBOX_SRC=$(cd .. && pwd)/pyo-sandbox-images
    if [ ! -f "$SANDBOX_SRC/images/base/Dockerfile" ] || [ ! -f "$SANDBOX_SRC/images/desktop/Dockerfile" ]; then
      echo "pyo-up: PYO_UP_SANDBOX_IMAGES=local builds from $SANDBOX_SRC, which isn't a pyo-sandbox-images checkout." >&2
      exit 2
    fi
    ;;
  *)
    echo "pyo-up: PYO_UP_SANDBOX_IMAGES is local or published, not $PYO_UP_SANDBOX_IMAGES." >&2
    exit 2
    ;;
esac

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
  # A dev or test machine: no install count unless PYO_TELEMETRY says so.
  exec sudo env PYO_TELEMETRY="${PYO_TELEMETRY:-off}" bash deploy/install.sh --version "$VERSION" "$@"
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
  exec env PYO_IMAGES="$IMAGES" PYO_SANDBOX_IMAGES_SRC="$SANDBOX_SRC" scripts/build-images.sh --dry-run "${BUILD_ARGS[@]}"
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

# A step as the install script shows one (pyo:progress): "→ label 0:12"
# while it runs, then "✓ label 0:12" or "✗ label".
SPIN_LABEL="" SPIN_T0=0 SPIN_N=0 SPIN_NEXT=0
spin_start() {
  SPIN_LABEL=$1 SPIN_T0=$SECONDS SPIN_N=0 SPIN_NEXT=$((SECONDS + 15)) SPINNING=1
  if [ -n "$TTY" ]; then printf '\033[?25l'; else printf '→ %s\n' "$SPIN_LABEL"; fi
}
spin_tick() {
  local el=$((SECONDS - SPIN_T0)) line cols
  if [ -n "$TTY" ]; then
    cols=$(tput cols 2>/dev/null) || cols=80
    [[ "$cols" =~ ^[0-9]+$ ]] && [ "$cols" -ge 20 ] || cols=80
    line="$SPIN_LABEL $(fmt_time "$el")"
    printf '\r\033[2K%s→%s %s' "$CYAN" "$RESET" "${line:0:$((cols - 3))}"
  elif [ "$SECONDS" -ge "$SPIN_NEXT" ]; then
    printf '→ %s %s\n' "$SPIN_LABEL" "$(fmt_time "$el")"
    SPIN_NEXT=$((SECONDS + 15))
  fi
  SPIN_N=$((SPIN_N + 1))
}
# Clears the spinner's line, to print something else.
spin_clear() { if [ -n "$TTY" ]; then printf '\r\033[2K'; fi; }
spin_stop() { # ok|fail, what to say, e.g. "pyo ready"
  spin_clear
  if [ -n "$TTY" ]; then printf '\033[?25h'; fi
  SPINNING=""
  if [ "$1" = ok ]; then
    printf '%s✓%s %s %s%s%s\n' "$GREEN" "$RESET" "$2" "$DIM" "$(fmt_time "$((SECONDS - SPIN_T0))")" "$RESET"
  else
    printf '%s✗ %s%s\n' "$RED" "$2" "$RESET"
  fi
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
  if [ "$rc" -eq 0 ]; then spin_stop ok "$label"; else spin_stop fail "$label"; fi
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
    spin_stop ok "${1#*/} ready"
    return 0
  fi
  spin_stop fail "${1#*/} isn't ready after $2"
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
  answer=""
  if [ -t 0 ]; then
    read -r -p "This machine isn't set up for Pyo yet. Set it up now? [y/N] " answer || answer=""
  fi
  case "$answer" in
    [yY] | [yY][eE][sS]) sudo bash scripts/setup-host.sh ;;
    *)
      echo "Set up this machine first: sudo bash scripts/setup-host.sh" >&2
      exit 1
      ;;
  esac
  if ! command -v kubectl >/dev/null || [ ! -r "$KUBECONFIG" ]; then
    echo "The setup didn't finish. Run it again: sudo bash scripts/setup-host.sh" >&2
    exit 1
  fi
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

# ── Where Pyo is, and this machine's helpers ───────────────────────────────
# One server needs no Tailscale: Pyo is on this machine's public IP over
# HTTPS (https://pyo.<public IP>.nip.io, Let's Encrypt) until an admin
# closes it, and over plain HTTP on :8000 from this machine, its private
# network and its tailnet. Once Tailscale is set up in Pyo (Integrations),
# this machine joins the tailnet by itself and Pyo is also
# https://pyo.<tailnet>.ts.net. Which addresses there are, and how they're
# shown, is in addresses.sh (host_units writes it).

# >>> pyo:progress (the same in deploy/install.sh, connect.sh, join.sh, scripts/pyo-up.sh and deploy/sandbox.sh)
# Steps, as a checklist. On a terminal, the step that runs ticks on one
# line ("→ Downloading k3s… 0:12"), then stays: done ("✓ Downloaded k3s
# 0:12", the time once it took a while), failed ("✗ Downloading k3s") or
# still going on its own ("● Starting the dashboard"), in color unless
# NO_COLOR is set. Elsewhere (piped, a CI log), no colors and no line
# drawn over: each step's line comes once (not again for a step with the
# same line), then with its time every 20 seconds while it runs, then how
# it ended (once for steps alike). progress_start MESSAGE, then
# progress_tick as often as it likes, then progress_end [ok|fail|wait]
# [WHAT IT DID]. And: say TEXT, a heading. ok_line TEXT, the last line
# once all went well. dot_line green|yellow|red|dim TEXT, how something
# is (running, starting, failed, stopped).
progress_color() { [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; }
# progress_paint SGR TEXT: TEXT in that color, when there are colors (it
# prints itself: in $(...) there's never a terminal).
progress_paint() {
  if progress_color; then printf '\033[%sm%s\033[0m' "$1" "$2"; else printf '%s' "$2"; fi
}
progress_time() { printf '%d:%02d' $(($1 / 60)) $(($1 % 60)); }
say() { progress_paint 1 "→ $*"; echo; }
ok_line() { progress_paint '1;32' "✓ $*"; echo; }
dot_line() {
  local c=2
  case $1 in green) c=32 ;; yellow) c=33 ;; red) c=31 ;; esac
  progress_paint "$c" ●
  printf ' %s\n' "$2"
}
progress_start() {
  PROGRESS_MSG=$1 PROGRESS_T0=$SECONDS PROGRESS_NEXT=0
  progress_tick
}
progress_tick() {
  [ -n "${PROGRESS_MSG:-}" ] || return 0
  local t=$((SECONDS - PROGRESS_T0))
  if [ -t 1 ]; then
    printf '\r\033[K'
    progress_paint 36 →
    printf ' %s ' "$PROGRESS_MSG"
    progress_paint 2 "$(progress_time "$t")"
  elif [ "$t" -ge "$PROGRESS_NEXT" ]; then
    if [ "$PROGRESS_NEXT" -gt 0 ]; then
      printf '→ %s %s\n' "$PROGRESS_MSG" "$(progress_time "$t")"
    elif [ "$PROGRESS_MSG" != "${PROGRESS_LAST:-}" ]; then
      printf '→ %s\n' "$PROGRESS_MSG"
    fi
    PROGRESS_LAST=$PROGRESS_MSG PROGRESS_NEXT=$((t + 20))
  fi
  return 0
}
# What a step did, from what it was doing: "Downloading k3s…" is
# "Downloaded k3s".
progress_done() {
  local m=${1%…} w
  w=${m%% *}
  case $w in
    Applying) w=Applied ;; Building) w=Built ;; Checking) w=Checked ;;
    Connecting) w=Connected ;; Copying) w=Copied ;; Creating) w=Created ;;
    Deleting) w=Deleted ;; Downloading) w=Downloaded ;; Getting) w=Got ;;
    Installing) w=Installed ;; Joining) w=Joined ;; Moving) w=Moved ;;
    Removing) w=Removed ;; Restarting) w=Restarted ;; Setting) w=Set ;;
    Starting) w=Started ;; Stopping) w=Stopped ;; Updating) w=Updated ;;
    Waiting) w=Waited ;;
  esac
  printf '%s%s' "$w" "${m#"${m%% *}"}"
}
progress_end() {
  [ -n "${PROGRESS_MSG:-}" ] || return 0
  local t=$((SECONDS - PROGRESS_T0)) done=""
  [ ! -t 1 ] || printf '\r\033[K'
  [ "${1:-ok}" != ok ] || done=${2:-$(progress_done "$PROGRESS_MSG")}
  # The same step again (a file each): said once.
  if [ -n "$done" ] && [ "$done" = "${PROGRESS_DONE:-}" ]; then
    PROGRESS_MSG=""
    return 0
  fi
  PROGRESS_DONE=$done
  case ${1:-ok} in
    ok)
      progress_paint 32 ✓
      printf ' %s' "$done"
      ;;
    fail)
      progress_paint 31 "✗ ${PROGRESS_MSG%…}"
      t=0
      ;;
    *)
      progress_paint 33 ●
      printf ' %s' "${2:-${PROGRESS_MSG%…}}"
      ;;
  esac
  if [ "$t" -ge 5 ]; then
    printf ' '
    progress_paint 2 "$(progress_time "$t")"
  fi
  echo
  PROGRESS_MSG=""
}
# wait_until [-f] MESSAGE SECONDS COMMAND...: COMMAND every 2 seconds
# until it succeeds, or a failure once SECONDS have passed: the step goes
# on by itself ("●"), or with -f it failed ("✗"). Nothing to show when it
# succeeds at once.
wait_until() {
  local end=wait
  [ "$1" != -f ] || { end=fail; shift; }
  local msg=$1 limit=$2 t0=$SECONDS
  shift 2
  "$@" && return 0
  progress_start "$msg"
  until "$@"; do
    if [ $((SECONDS - t0)) -ge "$limit" ]; then
      progress_end "$end"
      return 1
    fi
    sleep 1
    progress_tick
    sleep 1
    progress_tick
  done
  progress_end
}
# quietly MESSAGE COMMAND...: COMMAND in the background, its output kept
# aside and shown only when it fails.
quietly() {
  local log pid rc=0
  log=$(mktemp)
  progress_start "$1"
  shift
  "$@" >"$log" 2>&1 </dev/null &
  pid=$!
  while kill -0 "$pid" 2>/dev/null; do
    sleep 1
    progress_tick
  done
  wait "$pid" || rc=$?
  if [ "$rc" = 0 ]; then progress_end; else progress_end fail; fi
  [ "$rc" = 0 ] || cat "$log" >&2
  rm -f "$log"
  return "$rc"
}
# <<< pyo:progress
# Pyo's addresses (addresses.sh). Where this machine has a public IP of its
# own (on a network card, or the one its cloud maps to it), waits up to $1
# seconds (90 at most) for the HTTPS address there: until Let's Encrypt
# issued its certificate, or couldn't reach this machine. $2: the last
# line of the steps before them ("Pyo 0.4.3 is installed").
pyo_addresses() {
  local wait=${1:-0}
  # shellcheck source=/dev/null
  if ! declare -F addr_print >/dev/null && ! . "${PYO_ADDRESSES_SH:-/usr/local/lib/pyo/addresses.sh}" 2>/dev/null; then
    [ -z "${2:-}" ] || ok_line "$2"
    echo
    echo "Open Pyo at http://localhost:8000"
    return 0
  fi
  [ "$wait" -le 90 ] || wait=90
  [ "$wait" = 0 ] || addr_wait_wanted || wait=0
  if [ "$wait" != 0 ]; then
    ADDR_WAITED=1
    wait_until "Checking the HTTPS address…" "$wait" addr_public_settled || true
  fi
  [ -z "${2:-}" ] || ok_line "$2"
  echo
  # The dashboard didn't answer in time: it still comes up by itself.
  if [ -n "${STARTING:-}" ] && ! dashboard_up; then
    dot_line yellow "Pyo is still starting. It opens at these addresses in a minute or two."
  fi
  echo "Open Pyo:"
  addr_print open
  [ -n "$ADDR_HTTPS" ] || echo "For an HTTPS address, set up Tailscale in Pyo."
}
# The host's own helpers, as systemd timers (root), every minute: connecting
# this machine as a cluster until it's done, and putting it on the tailnet
# once Tailscale is set up in Pyo. Idempotent.
host_units() {
  local sudo=()
  [ "$(id -u)" = 0 ] || sudo=(sudo)
  "${sudo[@]}" install -d -m 755 /usr/local/lib/pyo
  "${sudo[@]}" install -d -m 700 /etc/pyo
  "${sudo[@]}" tee /usr/local/lib/pyo/connect-this-machine.sh >/dev/null <<'SH'
#!/usr/bin/env bash
# Connects the machine Pyo runs on as a cluster (its gateway serves Pyo over
# HTTPS), once a team exists: Pyo hands over the same command as "Add
# cluster › This machine", which fetches connect.sh from Pyo here (no
# internet needed). Stops itself once the agent has reported in and its
# admission policy, which holds it to its limits, is there.
set -euo pipefail
done_() { systemctl disable --now pyo-connect.timer >/dev/null 2>&1 || true; }
# The policy and its binding are there, or Kubernetes is older than 1.30.
policy_ok() {
  local minor
  k3s kubectl get validatingadmissionpolicy pyo-agent >/dev/null 2>&1 &&
    k3s kubectl get validatingadmissionpolicybinding pyo-agent >/dev/null 2>&1 && return 0
  minor=$({ k3s kubectl version -o json 2>/dev/null || true; } |
    sed -n '/"serverVersion"/,/}/s/^ *"minor": *"\([0-9][0-9]*\).*/\1/p')
  [ -n "$minor" ] && [ "$minor" -lt 30 ]
}
# One run at a time (the timer and the setup's own run): a second one waits,
# then finds this machine connected, or runs the same command again.
exec 9>/run/pyo-connect.lock
flock 9
key=$(cat /etc/pyo/bootstrap-key)
r=$(curl -fsS -m 20 "http://127.0.0.1:8000/install/this-machine?key=$key") || exit 0
case "$r" in
  *'"state":"connected"'*)
    # Without its policy (k3s didn't answer in time): connect.sh again, which
    # reads the agent's token from the cluster.
    if ! policy_ok; then
      curl -fsS -m 60 http://127.0.0.1:8000/install/connect.sh | bash
    fi
    if policy_ok; then done_; fi
    ;;
  *'"state":"connect"'*)
    cmd=$(sed -n 's/.*"command":"\([^"]*\)".*/\1/p' <<<"$r")
    [ -n "$cmd" ] || exit 0
    bash -c "$cmd"
    if policy_ok; then done_; fi
    ;;
esac
SH
  "${sudo[@]}" tee /usr/local/lib/pyo/tailnet-this-machine.sh >/dev/null <<'SH'
#!/usr/bin/env bash
# Tells Pyo this machine's Tailscale (its address and name, none when it's
# not on a tailnet). Once Tailscale is set up in Pyo, connect.sh (from Pyo
# here) puts this machine on the tailnet with the single-use key Pyo hands
# over, and moves k3s onto it with one restart. Each step is reported, so
# the dashboard shows it. A move that failed is tried again a day later.
set -euo pipefail
exec 9>/run/pyo-tailnet.lock
flock -n 9 || exit 0
key=$(cat /etc/pyo/bootstrap-key)
failed=/etc/pyo/k3s-tailnet-failed
ip="" name=""
look() {
  ip="" name=""
  command -v tailscale >/dev/null || return 0
  ip=$(tailscale ip -4 2>/dev/null | head -n1 || true)
  name=$(tailscale status --json 2>/dev/null | sed -n '/"Self"/,/}/s/^ *"DNSName": *"\([^"]*\)",*$/\1/p' | head -n1 || true)
}
# report [k3s state] [error]: Pyo's answer. Pyo may be restarting with k3s,
# so this tries for a few minutes.
report() {
  local i
  for i in $(seq 36); do
    curl -fsS -m 30 --data-urlencode "key=$key" --data-urlencode "ip=$ip" --data-urlencode "name=${name%.}" \
      --data-urlencode "k3s=${1:-}" --data-urlencode "error=${2:-}" \
      http://127.0.0.1:8000/install/this-machine/tailnet && return 0
    [ -n "${1:-}" ] || return 1
    sleep 5
  done
  return 1
}
# k3s isn't on this tailnet address yet.
k3s_off_tailnet() {
  command -v k3s >/dev/null || return 1
  ! grep -qx "node-ip: \"$ip\"" /etc/rancher/k3s/config.yaml.d/40-pyo-tailnet.yaml 2>/dev/null
}
move() { # [auth key]
  local out rc=0
  report moving >/dev/null || true
  out=$(curl -fsS -m 60 http://127.0.0.1:8000/install/connect.sh | TS_AUTHKEY="${1:-}" bash -s -- --tailnet-only 2>&1) || rc=$?
  printf '%s\n' "$out"
  look
  if [ "$rc" = 0 ]; then
    rm -f "$failed"
    report done >/dev/null || true
  else
    echo "$ip" > "$failed"
    report failed "$(grep -v '^ *$' <<<"$out" | tail -n1)" >/dev/null || true
  fi
}
look
r=$(report) || exit 0
case "$r" in
  *'"state":"join"'*)
    k=$(sed -n 's/.*"authKey":"\([^"]*\)".*/\1/p' <<<"$r")
    if [ -n "$k" ]; then move "$k"; fi
    ;;
  *'"state":"ok"'*)
    # On the tailnet, k3s not yet: unless that failed for this address in
    # the last day.
    if [ -n "$ip" ] && k3s_off_tailnet; then
      if ! grep -qx "$ip" "$failed" 2>/dev/null || [ -n "$(find "$failed" -mmin +1440 2>/dev/null)" ]; then
        move
      fi
    fi
    ;;
esac
# The host setup of the Pyo running now, once per version: an update from
# the dashboard brings new scripts and timers here too. Last, since it
# rewrites this script.
units=$(curl -fsS -m 30 http://127.0.0.1:8000/install/host-units.sh) || exit 0
sum=$(sha256sum <<<"$units" | cut -d' ' -f1)
[ "$sum" != "$(cat /etc/pyo/host-units 2>/dev/null)" ] || exit 0
exec bash -c "$units"$'\n'"echo $sum > /etc/pyo/host-units"
SH
  "${sudo[@]}" tee /usr/local/lib/pyo/addresses.sh >/dev/null <<'ADDR'
# Where Pyo is, as the install script, scripts/pyo-up.sh and pyo-reset-login
# show it. Sourced: it only defines functions.
#
#   Public   https://pyo.<public IP>.nip.io, with Let's Encrypt's
#            certificate. The public IP is this machine's own: on a network
#            card, or the one its cloud maps to it (1:1 NAT), from the
#            cloud's metadata. Never plain HTTP there: passwords would cross
#            the internet in the clear.
#   Tailnet  Pyo's tailnet name, else this machine's Tailscale IP on :8000
#            when Pyo answers there.
#   Network  this machine's private address on :8000: its main network
#            card's. Never a container, VM or k3s bridge's.
#   Local    http://localhost:8000

# Public IPv4 addresses among the lines on stdin: not private, tailnet
# (CGNAT), loopback, link-local, multicast or reserved.
addr_public_v4s() {
  grep -E '^((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\.){3}(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])$' |
    grep -vE '^(0|10|127|2(2[4-9]|[3-5][0-9]))\.|^169\.254\.|^192\.168\.|^172\.(1[6-9]|2[0-9]|3[01])\.|^100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.' || true
}

# This machine's IPv4 addresses on its own network cards, "card address" a
# line. Not loopback, tunnels (Tailscale's comes on its own), or the
# bridges of containers, VMs and k3s (docker, br-*, cni0, flannel, libvirt,
# multipass, LXD), nor k3s's pod and service ranges or docker's own.
addr_own_ipv4s() {
  local out
  if out=$(ip -o -4 addr show 2>/dev/null) && [ -n "$out" ]; then
    awk '{ split($4, a, "/"); sub(/@.*/, "", $2); print $2, a[1] }' <<<"$out"
  else
    hostname -I 2>/dev/null | tr ' ' '\n' | grep -E '^[0-9]+(\.[0-9]+){3}$' | sed 's/^/- /'
  fi | grep -vE '^(lo|docker[0-9]*|br-[^ ]*|veth[^ ]*|cni[0-9]*|flannel[^ ]*|cali[^ ]*|vxlan[^ ]*|kube-[^ ]*|cilium[^ ]*|virbr[^ ]*|mpqemubr[^ ]*|lxdbr[^ ]*|lxcbr[^ ]*|podman[^ ]*|tailscale[^ ]*|tun[^ ]*|wg[^ ]*|zt[^ ]*) ' |
    grep -vE ' (127\.|169\.254\.|10\.4[23]\.|172\.17\.)' || true
}

# A public IPv4 address on this machine's network cards (as Pyo's agent
# finds them: locate.rs).
addr_nic_public_ip() {
  addr_own_ipv4s | cut -d' ' -f2 | addr_public_v4s | head -n1
}

# This machine on its private network: an address of the card the default
# route goes through, else the first private one. One address, or none.
addr_network_ip() {
  local all dev
  all=$(addr_own_ipv4s | grep -E ' (10\.|192\.168\.|172\.(1[6-9]|2[0-9]|3[01])\.)' || true)
  dev=$(ip -4 route show default 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -n1 || true)
  { [ -z "$dev" ] || grep "^$dev " <<<"$all" || true; echo "$all"; } | sed -n '1s/^[^ ]* //p'
}

# One cloud's metadata service: the public IPv4 address it maps to this
# machine. Each request waits a second at most.
addr_cloud_ask() {
  local m=169.254.169.254 t="" h=()
  case $1 in
    aws) # AWS (IMDSv2, else v1), and clouds that answer like it
      t=$(curl -fsS -m 1 -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' "http://$m/latest/api/token" 2>/dev/null) || t=""
      [ -z "$t" ] || h=(-H "X-aws-ec2-metadata-token: $t")
      curl -fsS -m 1 "${h[@]}" "http://$m/latest/meta-data/public-ipv4" ;;
    gcp) curl -fsS -m 1 -H 'Metadata-Flavor: Google' "http://$m/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip" ;;
    azure) curl -fsS -m 1 -H 'Metadata: true' "http://$m/metadata/instance/network/interface/0/ipv4/ipAddress/0/publicIpAddress?api-version=2021-02-01&format=text" ;;
    alibaba)
      t=$(curl -fsS -m 1 -X PUT -H 'X-aliyun-ecs-metadata-token-ttl-seconds: 60' http://100.100.100.200/latest/api/token 2>/dev/null) || t=""
      [ -z "$t" ] || h=(-H "X-aliyun-ecs-metadata-token: $t")
      curl -fsS -m 1 "${h[@]}" http://100.100.100.200/latest/meta-data/eipv4 ||
        curl -fsS -m 1 "${h[@]}" http://100.100.100.200/latest/meta-data/public-ipv4 ;;
    tencent) curl -fsS -m 1 http://169.254.0.23/latest/meta-data/public-ipv4 ;;
    scaleway) curl -fsS -m 1 http://169.254.42.42/conf | sed -n 's/^PUBLIC_IP_ADDRESS=//p' ;;
  esac 2>/dev/null
  echo
}

# The public IPv4 address this machine's cloud maps to it, from the cloud's
# metadata: every cloud asked at once, so it takes a second or two. Empty
# elsewhere (a home network, a laptop, a VM). Never what the internet sees
# of this machine's own requests: that can be a router's or a shared
# gateway's address, which doesn't lead back here.
addr_cloud_ip() {
  local d c pids=()
  d=$(mktemp -d)
  for c in aws gcp azure alibaba tencent scaleway; do
    addr_cloud_ask "$c" >"$d/$c" &
    pids+=($!)
  done
  wait "${pids[@]}" 2>/dev/null || true
  cat "$d"/* 2>/dev/null | tr -d '\r ' | addr_public_v4s | head -n1
  rm -rf "$d"
}

# Pyo's agent's word for it: the public IP this machine's cloud maps to it,
# as its locate.rs found it (a cloud whose metadata doesn't say, like
# Oracle's).
addr_agent_cloud_ip() {
  addr_kc get node "$(hostname | tr 'A-Z' 'a-z')" -o jsonpath='{.metadata.annotations.pyo/cloud-public-ipv4}' 2>/dev/null |
    addr_public_v4s | head -n1
}

# This machine's own public IPv4 address (looked up once): on a network
# card, else the one its cloud maps to it. Empty without one.
addr_public_ip() {
  if [ -z "${ADDR_IP_KNOWN:-}" ]; then
    ADDR_IP=$(addr_nic_public_ip)
    [ -n "$ADDR_IP" ] || ADDR_IP=$(addr_cloud_ip)
    [ -n "$ADDR_IP" ] || ADDR_IP=$(addr_agent_cloud_ip)
    ADDR_IP_KNOWN=1
  fi
  echo "$ADDR_IP"
}

addr_kc() {
  if [ "$(id -u)" = 0 ]; then k3s kubectl "$@"; else kubectl "$@"; fi
}

# The certificate of Pyo's HTTPS address on the public IP, as cert-manager
# has it: ADDR_CERT_HOST ("pyo.<public IP>.nip.io", empty when Pyo asked
# for none) and ADDR_CERT_STATE: ready, failed (Let's Encrypt couldn't
# reach this machine on port 80), or pending. A challenge or order Let's
# Encrypt turned down counts as failed at once, before the certificate
# says so.
addr_public_cert() {
  local host ready reason made ns=pyo-gateway
  ADDR_CERT_HOST="" ADDR_CERT_STATE="" ADDR_CERT_FAILED_AT=""
  read -r host ready reason made ADDR_CERT_FAILED_AT < <(addr_kc -n "$ns" get certificate pyo-tls---dashboard \
    -o jsonpath='{.spec.dnsNames[0]} {.status.conditions[?(@.type=="Ready")].status}- {.status.conditions[?(@.type=="Issuing")].reason}- {.metadata.creationTimestamp} {.status.lastFailureTime}' 2>/dev/null || true) || true
  [ -n "$host" ] || return 0
  ADDR_CERT_HOST=$host ADDR_CERT_STATE=pending
  if [ "$ready" = True- ]; then
    ADDR_CERT_STATE=ready
  elif [ "$reason" = Failed- ] || [ -n "$ADDR_CERT_FAILED_AT" ] ||
    addr_kc -n "$ns" get challenges.acme.cert-manager.io,orders.acme.cert-manager.io \
      -o jsonpath='{range .items[*]}{.metadata.creationTimestamp} {.spec.dnsName}{.spec.dnsNames[0]} {.status.state}{"\n"}{end}' 2>/dev/null |
    awk -v host="$host" -v made="$made" '$1 >= made && $2 == host && ($3 == "invalid" || $3 == "errored") { found = 1 } END { exit !found }'; then
    ADDR_CERT_STATE=failed
  fi
}

# A certificate Let's Encrypt refused 15 minutes ago or more is asked for
# again now (Pyo's agent makes it again at once): running the installer
# again after opening the ports checks them. Not sooner: Let's Encrypt
# allows 5 failed checks for a name an hour.
addr_retry_failed() {
  local at
  [ "${ADDR_CERT_STATE:-}" = failed ] && [ -n "${ADDR_CERT_FAILED_AT:-}" ] || return 1
  at=$(date -d "$ADDR_CERT_FAILED_AT" +%s 2>/dev/null) || return 1
  [ $(($(date +%s) - at)) -ge 900 ] || return 1
  addr_kc -n pyo-gateway delete certificate pyo-tls---dashboard --wait=false >/dev/null 2>&1 || return 1
  ADDR_CERT_STATE=pending ADDR_CERT_FAILED_AT=""
}

# Done waiting for the HTTPS address: its certificate is issued, or Let's
# Encrypt couldn't reach this machine.
addr_public_settled() {
  addr_public_cert
  [ "$ADDR_CERT_STATE" = ready ] || [ "$ADDR_CERT_STATE" = failed ]
}

# Worth waiting for the HTTPS address: this machine has a public IP of its
# own, and the certificate there isn't settled (or was refused a while ago:
# tried again). Without one asked for yet, not when Pyo is on the tailnet
# (an admin may have closed it).
addr_wait_wanted() {
  ! addr_public_settled || addr_retry_failed || return 1
  [ -n "$(addr_public_ip)" ] || return 1
  [ -n "$ADDR_CERT_HOST" ] || [ -z "$(addr_pyo_tailnet)" ]
}

# Pyo's HTTPS name on the tailnet ("https://pyo.<tailnet>.ts.net"), once it
# has one.
addr_pyo_tailnet() {
  curl -fsS -m 5 http://127.0.0.1:8000/api/v1/meta 2>/dev/null |
    sed -n 's/.*"publicUrl":"\(https:\/\/[^"]*\.ts\.net\)\/*".*/\1/p' | head -n1 || true
}

# Pyo on the tailnet: its HTTPS name there, else this machine's Tailscale
# IP on :8000, when Tailscale is up here and Pyo answers on it.
addr_tailnet() {
  local url ip
  url=$(addr_pyo_tailnet)
  if [ -n "$url" ]; then
    echo "$url"
    return 0
  fi
  command -v tailscale >/dev/null 2>&1 || return 0
  ip=$(tailscale ip -4 2>/dev/null | head -n1 || true)
  [ -n "$ip" ] || return 0
  curl -fsS -m 3 -o /dev/null "http://$ip:8000/api/v1/meta" 2>/dev/null && echo "http://$ip:8000"
  return 0
}

# Colors on a terminal, unless NO_COLOR says not to.
addr_color() { [ -z "${NO_COLOR:-}" ] && [ -t 1 ]; }

# addr_line LABEL URL [NOTE]: "  ➜  Public:   https://…  (note)", the
# labels lined up. On a terminal: a green arrow, the label in bold, the
# address in cyan.
addr_line() {
  local pad note=""
  pad=$(printf '%*s' $((9 - ${#1})) '')
  [ -z "${3:-}" ] || note="  ($3)"
  if addr_color; then
    printf '  \033[32m➜\033[0m  \033[1m%s:\033[0m%s\033[36m%s\033[0m%s\n' "$1" "$pad" "$2" "$note"
  else
    printf '  ➜  %s:%s%s%s\n' "$1" "$pad" "$2" "$note"
  fi
}

# addr_print open|PATH: Pyo's addresses, one line each. "open": for opening
# Pyo, with the HTTPS address on the public IP also while it's on its way
# or blocked, and what to do then. A PATH: links to it that work now.
# ADDR_HTTPS says whether there's a Public or Tailnet line.
addr_print() {
  local path="" host="" ip url net note=""
  [ "${1:-open}" = open ] || path=$1
  ADDR_HTTPS=""
  [ -n "${ADDR_CERT_STATE:-}" ] || addr_public_cert
  host=$ADDR_CERT_HOST
  # None asked for yet, after a wait for it: it's on its way.
  if [ -z "$host" ] && [ -n "${ADDR_WAITED:-}" ]; then
    ip=$(addr_public_ip)
    [ -z "$ip" ] || host="pyo.$ip.nip.io"
  fi
  if [ -n "$host" ]; then
    case "${ADDR_CERT_STATE:-}" in
      ready) ;;
      failed) note="allow ports 80 and 443 in your cloud firewall" ;;
      *) note="ready in a few minutes" ;;
    esac
    if [ -z "$note" ] || [ -z "$path" ]; then
      addr_line Public "https://$host$path" "$note"
      ADDR_HTTPS=1
    fi
  fi
  url=$(addr_tailnet)
  if [ -n "$url" ]; then
    addr_line Tailnet "$url$path"
    ADDR_HTTPS=1
  fi
  net=$(addr_network_ip)
  [ -z "$net" ] || addr_line Network "http://$net:8000$path"
  addr_line Local "http://localhost:8000$path"
}
ADDR
  "${sudo[@]}" tee /usr/local/lib/pyo/pyo-reset-login >/dev/null <<'SH'
#!/usr/bin/env bash
# Prints a one-time link to choose a new password for a Pyo account, for a
# password that's lost (the only admin's too). On the server Pyo runs on:
#
#   sudo pyo-reset-login [EMAIL]
#
# Without an email it's for Pyo's admin. Pyo makes the link with this
# machine's install key (recovery.rs). It works once, for 30 minutes.
set -euo pipefail
[ "$(id -u)" = 0 ] || exec sudo "$0" "$@"
case "${1:-}" in
  -h | --help) echo "Usage: sudo pyo-reset-login [EMAIL]"; exit 0 ;;
esac
key=$(cat /etc/pyo/bootstrap-key 2>/dev/null) || {
  echo "Pyo isn't installed on this machine. Run this on the server Pyo runs on." >&2
  exit 1
}
out=$(curl -sS -m 30 -w '\n%{http_code}' --data-urlencode "key=$key" --data-urlencode "email=${1:-}" \
  http://127.0.0.1:8000/install/reset-login 2>/dev/null) || out=""
status=${out##*$'\n'} body=${out%$'\n'*}
field() { sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p" <<<"$body" | head -n1; }
if [ -z "$out" ]; then
  echo "Pyo isn't answering on this machine. Wait a minute, then run this again." >&2
  exit 1
fi
if [ "$status" != 200 ]; then
  msg=$(field error)
  [ -n "$msg" ] || msg="Pyo couldn't make a sign-in link. Run this again."
  echo "$msg" >&2
  exit 1
fi
path=$(field path) email=$(field email)
minutes=$(sed -n 's/.*"minutes":\([0-9]*\).*/\1/p' <<<"$body")
# shellcheck source=/dev/null
. /usr/local/lib/pyo/addresses.sh
echo "Open one of these links to choose a new password for $email:"
addr_print "$path"
echo "It works once, for ${minutes:-30} minutes."
SH
  "${sudo[@]}" chmod 755 /usr/local/lib/pyo/connect-this-machine.sh /usr/local/lib/pyo/tailnet-this-machine.sh /usr/local/lib/pyo/pyo-reset-login
  "${sudo[@]}" chmod 644 /usr/local/lib/pyo/addresses.sh
  "${sudo[@]}" ln -sfn /usr/local/lib/pyo/pyo-reset-login /usr/local/bin/pyo-reset-login
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
  "${sudo[@]}" tee /etc/systemd/system/pyo-tailnet.service >/dev/null <<'UNIT'
[Unit]
Description=Pyo: this machine on the tailnet
After=network-online.target k3s.service tailscaled.service
[Service]
Type=oneshot
ExecStart=/usr/local/lib/pyo/tailnet-this-machine.sh
UNIT
  "${sudo[@]}" tee /etc/systemd/system/pyo-tailnet.timer >/dev/null <<'UNIT'
[Unit]
Description=Pyo: put this machine on the tailnet once Tailscale is set up
[Timer]
OnBootSec=1min
OnUnitActiveSec=1min
[Install]
WantedBy=timers.target
UNIT
  "${sudo[@]}" systemctl daemon-reload
  "${sudo[@]}" systemctl -q enable --now pyo-connect.timer pyo-tailnet.timer
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
PLATFORM=$(cat deploy/k8s/platform.yaml)
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
  for image in pyo agent builder flows db-controller kv static waf geoip postgres-16 postgres-17 postgres-18 mysql-operator mysql-backup; do
    echo "$image $NO_BUILD_TAG"
  done >"$TAGS"
else
  BUILDCTL="$BUILDCTL" PYO_IMAGES="$IMAGES" PYO_BUILDKIT_ADDR=tcp://127.0.0.1:1234 PYO_SANDBOX_IMAGES_SRC="$SANDBOX_SRC" \
    scripts/build-images.sh --tag "$(date +%Y%m%d%H%M%S)" --out "$TAGS" "${BUILD_ARGS[@]}" "${VERBOSE[@]}"
  kubectl -n "$NS" scale deployment/buildkitd --replicas=0 >/dev/null
  BUILDKIT_UP=""
fi
declare -A TAG_OF=()
while read -r image tag; do TAG_OF[$image]=$tag; done <"$TAGS"
TAG=${TAG_OF[pyo]}

# ── Deploy ─────────────────────────────────────────────────────────────────
log "Deploying pyo:$TAG"
# Every image at its own tag (postgres:<version>-<tag>).
IMAGE_SED=()
for image in "${!TAG_OF[@]}"; do
  case "$image" in
    postgres-*) IMAGE_SED+=(-e "s|__IMAGES__/postgres:${image#postgres-}-__TAG__|$IMAGES/postgres:${image#postgres-}-${TAG_OF[$image]}|g") ;;
    *) IMAGE_SED+=(-e "s|__IMAGES__/$image:__TAG__|$IMAGES/$image:${TAG_OF[$image]}|g") ;;
  esac
done
# Pyo's built-in sandbox templates take their images from
# upsetdev/pyo-sandbox-images' images.json (sandbox/images.rs). Built here
# (PYO_UP_SANDBOX_IMAGES=local), Pyo is pinned to them by digest instead,
# and sandbox servers pull them from Pyo's registry over the tailnet.
pinned_ref() {
  local digest
  digest=$(curl -fsS -m 10 -o /dev/null -D - \
    -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json' \
    "http://${IMAGES%%/*}/v2/${IMAGES#*/}/$1/manifests/$2" 2>/dev/null \
    | tr -d '\r' | awk 'tolower($1) == "docker-content-digest:" { print $2 }') || digest=""
  [ -z "$digest" ] || echo "$IMAGES/$1:$2@$digest"
}
SANDBOX_BASE_REF="" SANDBOX_DESKTOP_REF=""
if [ -n "$SANDBOX_SRC" ] && [ -n "${TAG_OF[sandbox-base]:-}" ] && [ -n "${TAG_OF[sandbox-desktop]:-}" ]; then
  SANDBOX_BASE_REF=$(pinned_ref sandbox-base "${TAG_OF[sandbox-base]}")
  SANDBOX_DESKTOP_REF=$(pinned_ref sandbox-desktop "${TAG_OF[sandbox-desktop]}")
  log "Sandbox templates: base and desktop from $SANDBOX_SRC"
fi
IMAGE_SED+=(-e "s|__SANDBOX_BASE_REF__|$SANDBOX_BASE_REF|g" -e "s|__SANDBOX_DESKTOP_REF__|$SANDBOX_DESKTOP_REF|g")
PYO_YAML=$(sed "${IMAGE_SED[@]}" -e "s|__IMAGES__|$IMAGES|g" deploy/k8s/pyo.yaml)
if grep -q "__TAG__" <<<"$PYO_YAML"; then
  echo "pyo-up: deploy/k8s/pyo.yaml has an image scripts/build-images.sh doesn't build:" >&2
  grep "__TAG__" <<<"$PYO_YAML" >&2
  exit 1
fi
"${APPLY[@]}" <<<"$PYO_YAML" >/dev/null || { echo "pyo-up: applying Pyo failed" >&2; exit 1; }
wait_rollout deployment/pyo 300s
wait_rollout deployment/pyo-worker 300s
wait_rollout deployment/pyo-forward 300s

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

# This machine as a cluster: its gateway serves Pyo over HTTPS on the
# public IP, which the addresses wait for.
host_units
as_root
run_step "Connecting this machine to Pyo" "${SUDO[@]}" /usr/local/lib/pyo/connect-this-machine.sh || true
sed 's/^/    | /' "$STEP_OUT"

pyo_addresses 120 "Pyo $TAG is up"
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
