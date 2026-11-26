#!/usr/bin/env bash
# Makes this machine a Pyo cluster: k3s (if it isn't installed) and the
# Pyo agent. Pyo's "Add cluster" gives you the full command, e.g.:
#
#   curl -fsSL https://your-pyo/install/connect.sh | bash -s -- \
#     --api https://pyo.tail1234.ts.net --token 3f9a... --tailscale-key tskey-auth-... \
#     [--registry server-1.tail1234.ts.net:5006 --registry-token 9b1c...]
#
#   --api            where the agent reaches Pyo
#   --token          the connect token (works once, within an hour)
#   --install-url    where this server gets Pyo's install files, when the
#                    agent reaches Pyo at another address (Pyo Cloud)
#   --tailscale-key  joins your tailnet first (a single-use key from Pyo)
#   --registry       Pyo's registry door, for images Pyo builds
#   --registry-token this cluster's login there (pull only)
#   --local-registry a registry in this cluster for the images it builds,
#                    instead of Pyo's (Pyo Cloud: images stay on your servers)
#   --version        runs this Pyo release's connect.sh instead, from pyo.dev
#   --auto-update    sets up automatic updates on this server: Pyo's verifier,
#                    which takes only signed Pyo releases
#   --no-auto-update turns them off. Without either, the last choice stays
#
# Or only joins this machine to the tailnet (Pyo's own machine does this
# once Tailscale is set up in Pyo): --tailnet-only, with TS_AUTHKEY. With
# --tailnet SUFFIX too, a machine on another tailnet leaves it for that one
# (a single-server cluster moving to its team's tailnet).
#
# Or removes what Pyo put into this cluster, leaving the cluster itself
# (k3s stays):
#
#   curl -fsSL https://your-pyo/install/connect.sh | bash -s -- --uninstall
#
#   --keep-data      keep project namespaces, databases and their volumes;
#                    the volumes of what's removed keep their data on disk
#   --yes            don't ask first
#   --dry-run        only say what it would delete
# With KUBECONFIG set, it uses kubectl and that cluster instead of k3s's.
# >>> pyo:release (the same in connect.sh, join.sh and deploy/sandbox.sh)
# With --version naming another Pyo release, runs that release's own
# script from pyo.dev, which keeps each release's scripts at
# https://pyo.dev/v/<version>/. Pyo 0.4.2 and earlier give pyo.dev's latest
# script their --version. The release workflow writes the release a script
# belongs to on the line marked pyo:release-version, in the copy it
# publishes. Pyo's own copy (<pyo>/install/) leaves it empty and never runs
# another.
run_release() {
  local name=$1 want="" prev="" arg dir rc=0
  local release="" # pyo:release-version
  shift
  for arg in "$@"; do
    [ "$prev" != --version ] || want=${arg#v}
    prev=$arg
  done
  [ -n "$release" ] && [ -n "$want" ] && [ "$want" != "$release" ] || return 0
  if [[ ! $want =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "--version takes a Pyo release, like $release. \"$want\" isn't one." >&2
    exit 2
  fi
  dir=$(mktemp -d)
  if ! curl -fsSL --proto '=https' --retry 3 -o "$dir/$name" "https://pyo.dev/v/$want/$name"; then
    rm -rf "$dir"
    echo "pyo.dev has no $name for Pyo $want. Copy the command from your Pyo again." >&2
    exit 1
  fi
  # Piped into bash, the rest of this script is still in the pipe. Read it
  # out, so curl can finish and the other script doesn't take it as input.
  [ -f "${BASH_SOURCE[1]:-}" ] || [ -t 0 ] || cat >/dev/null
  bash "$dir/$name" "$@" || rc=$?
  rm -rf "$dir"
  exit "$rc"
}
# <<< pyo:release
run_release connect.sh "$@"
# >>> pyo:become-root (the same in connect.sh and join.sh)
# Root, through sudo when it isn't: the script runs again as root, with
# the same arguments and Tailscale key. Run from a file, sudo runs that
# file. Piped into bash (curl ... | bash), the new shell reads the rest of
# the script from the same pipe, where bash left it: just after the line
# that calls this. So it's called before anything else runs, and before
# `set -euo pipefail`, which the rest sets again.
become_root() {
  [ "$(id -u)" = 0 ] && return 0
  if ! command -v sudo >/dev/null; then
    echo "Pyo needs root, and there's no sudo here. Run this as root." >&2
    exit 1
  fi
  local self=${BASH_SOURCE[1]:-}
  echo "Pyo needs root. Asking sudo..." >&2
  if [ -n "$self" ] && [ -f "$self" ]; then
    exec sudo env TS_AUTHKEY="${TS_AUTHKEY:-}" bash "$self" "$@"
  elif [ ! -t 0 ]; then
    exec sudo env TS_AUTHKEY="${TS_AUTHKEY:-}" bash -s -- "$@"
  fi
  echo "Run this as root (sudo)." >&2
  exit 1
}
# <<< pyo:become-root
# Before anything else is defined: piped in, the root shell reads on
# from here, without what came before.
# --uninstall uses kubectl as you with KUBECONFIG set, or without k3s here.
case " $* " in *" --uninstall "*) [ -n "${KUBECONFIG:-}" ] || ! command -v k3s >/dev/null || become_root "$@" ;; *) become_root "$@" ;; esac
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
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

# >>> pyo:connect-args (connect.sh only; join.rs tests it)
API="" TOKEN="" TS_AUTHKEY="${TS_AUTHKEY:-}" REGISTRY="" REGISTRY_TOKEN="" LOCAL_REGISTRY="" INSTALL_URL=""
UNINSTALL="" KEEP_DATA="" YES="" DRY_RUN="" WITH_PYO="" TAILNET_ONLY="" AUTO_UPDATE="" TAILNET=""
while [ $# -gt 0 ]; do
  case "$1" in
    --api) API="${2%/}"; shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    --install-url) INSTALL_URL="${2%/}"; shift 2 ;;
    --tailscale-key) TS_AUTHKEY="$2"; shift 2 ;;
    --registry) REGISTRY="$2"; shift 2 ;;
    --registry-token) REGISTRY_TOKEN="$2"; shift 2 ;;
    --local-registry) LOCAL_REGISTRY=1; shift ;;
    --tailnet-only) TAILNET_ONLY=1; shift ;;
    --tailnet) TAILNET="${2%.}"; shift 2 ;;
    --auto-update) AUTO_UPDATE=on; shift ;;
    --no-auto-update) AUTO_UPDATE=off; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --keep-data) KEEP_DATA=1; shift ;;
    --yes | -y) YES=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    # install.sh --uninstall on a k3s Pyo didn't install: Pyo itself goes too.
    --with-pyo) WITH_PYO=1; shift ;;
    # run_release (above) handled it.
    --version) shift 2 || shift ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done
# <<< pyo:connect-args
# >>> pyo:connect-lock (connect.sh only; join.rs tests it)
# One Pyo setup at a time on a server: this script and Pyo's verifier
# (`pyo-verify run` skips its turn while the lock is held). On the server
# Pyo runs on, also Pyo's own timers and installer (install.sh
# `host_units`). They hold this lock while they run connect.sh, which then
# has it as file 9. A run by hand waits for them. The lock goes when this
# script ends. The verifier is started without file 9, so nothing that
# stays behind holds the lock.
CONNECT_LOCK=/run/pyo-connect.lock
connect_lock_held() { [ "$(readlink "/proc/$$/fd/9" 2>/dev/null)" = "$CONNECT_LOCK" ]; }
connect_lock() {
  [ "$(id -u)" = 0 ] && command -v flock >/dev/null || return 0
  ! connect_lock_held || return 0
  exec 9>"$CONNECT_LOCK"
  flock -n 9 && return 0
  echo "Waiting for another Pyo setup on this server to finish."
  flock 9
}
# <<< pyo:connect-lock
# >>> pyo:auto-update (connect.sh only; join.rs tests it)
# Automatic updates on a cluster's first server: Pyo's verifier
# (pyo-verify), a small root program a systemd timer runs. It takes only
# what a signed Pyo release defines, for the version Pyo names.
# --auto-update turns it on, --no-auto-update off. Without either, the
# choice of the last run stays (AUTO_UPDATE_FILE). The server Pyo itself
# runs on has no verifier: it updates with Pyo.
#
# The release workflow writes this script's release and the SHA-256 of that
# release's verifier on the pyo:verify-pin lines, in the copy it publishes
# (scripts/release-manifest.ts). The verifier comes from that GitHub
# release and is installed only when it matches. A copy without the lines
# (Pyo's own at <pyo>/install/, a dev build) installs no verifier. Nothing
# here stops a connect: a step that fails says so in one line.
PYO_VERIFY_RELEASE="" # pyo:verify-pin release
PYO_VERIFY_SHA256_X86_64="" # pyo:verify-pin x86_64
PYO_VERIFY_SHA256_AARCH64="" # pyo:verify-pin aarch64
PYO_VERIFY_RELEASES=https://github.com/upsetdev/pyo/releases/download
AUTO_UPDATE_FILE=/etc/pyo/auto-update
VERIFY_BIN=/usr/local/bin/pyo-verify
VERIFY_UNITS=/etc/systemd/system
OWN_SERVER=/usr/local/lib/pyo/connect-this-machine.sh
# AUTO_UPDATE for this run: the flag, kept for later runs, else the choice
# of the last run.
auto_update_choice() {
  if [ -e "$OWN_SERVER" ]; then
    AUTO_UPDATE=""
  elif [ -n "$AUTO_UPDATE" ]; then
    { mkdir -p "$(dirname "$AUTO_UPDATE_FILE")" && echo "$AUTO_UPDATE" > "$AUTO_UPDATE_FILE"; } 2>/dev/null || true
  else
    AUTO_UPDATE=$(cat "$AUTO_UPDATE_FILE" 2>/dev/null || true)
    case "$AUTO_UPDATE" in on | off) ;; *) AUTO_UPDATE="" ;; esac
  fi
}
# This machine's processor, as the verifier's files name it.
verify_arch() {
  case "$(uname -m)" in
    x86_64 | amd64) echo x86_64 ;;
    aarch64 | arm64) echo aarch64 ;;
  esac
}
verify_pin() {
  case "$(verify_arch)" in
    x86_64) echo "$PYO_VERIFY_SHA256_X86_64" ;;
    aarch64) echo "$PYO_VERIFY_SHA256_AARCH64" ;;
  esac
}
verify_version() { [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]]; }
# verify_older A B: release A is older than B. A release candidate
# (X.Y.Z-rc.N) comes before its release.
verify_older() {
  local a=${1%%-*} b=${2%%-*} ra=999999999 rb=999999999 x y i
  verify_version "$1" && verify_version "$2" || return 1
  [ "$1" = "$a" ] || ra=${1##*-rc.}
  [ "$2" = "$b" ] || rb=${2##*-rc.}
  IFS=. read -r -a x <<<"$a.$ra"
  IFS=. read -r -a y <<<"$b.$rb"
  for i in 0 1 2 3; do
    [ "${x[i]}" -eq "${y[i]}" ] || { [ "${x[i]}" -lt "${y[i]}" ]; return; }
  done
  return 1
}
# verify_step COMMAND...: COMMAND in the background while the step's time
# ticks, quiet, and without the lock's file.
verify_step() {
  local pid rc=0
  "$@" >/dev/null 2>&1 </dev/null 9>&- &
  pid=$!
  while kill -0 "$pid" 2>/dev/null; do
    sleep 1
    progress_tick
  done
  wait "$pid" || rc=$?
  return "$rc"
}
# verify_fetch URL SHA256 FILE: 1 when the download fails, 2 when it isn't
# the pinned file.
verify_fetch() {
  local i got=""
  for i in 1 2 3; do
    curl -fsSL --proto '=https' --max-time 300 --max-filesize 67108864 -o "$3" "$1" && got=1 && break
    [ "$i" = 3 ] || sleep 2
  done
  [ -n "$got" ] || return 1
  [ "$(sha256sum "$3" | cut -d' ' -f1)" = "$2" ] || return 2
}
# Sets the verifier up for Pyo at URL, once the agent runs.
auto_update_on() {
  local url=$1 arch want have="" dir new rc=0 channel=()
  arch=$(verify_arch)
  want=$(verify_pin)
  if ! verify_version "$PYO_VERIFY_RELEASE" || [[ ! $want =~ ^[0-9a-f]{64}$ ]]; then
    [ -x "$VERIFY_BIN" ] || dot_line yellow "Automatic updates aren't available here. They stay off on this server."
    return 0
  fi
  # The verifier's install doesn't take the lock: this script holds it.
  if ! connect_lock_held; then
    dot_line yellow "Automatic updates couldn't be set up on this server."
    return 0
  fi
  [[ $PYO_VERIFY_RELEASE != *-rc.* ]] || channel=(--channel rc)
  progress_start "Setting up automatic updates…"
  [ ! -x "$VERIFY_BIN" ] || have=$("$VERIFY_BIN" version 2>/dev/null 9>&- || true)
  if [ "$have" = "$PYO_VERIFY_RELEASE" ] || verify_older "$PYO_VERIFY_RELEASE" "$have"; then
    # This release's verifier, or a later one it moved to: kept.
    new=$VERIFY_BIN
  else
    dir=$(mktemp -d)
    verify_step verify_fetch "$PYO_VERIFY_RELEASES/v$PYO_VERIFY_RELEASE/pyo-verify-linux-$arch" "$want" "$dir/pyo-verify" || rc=$?
    if [ "$rc" != 0 ]; then
      rm -rf "$dir"
      if [ "$rc" = 2 ]; then
        progress_end wait "Automatic updates aren't set up. The download didn't match its checksum."
      else
        progress_end wait "Automatic updates aren't set up. The download failed."
      fi
      return 0
    fi
    # Beside its place, not in /tmp, which may not run programs.
    new=$(dirname "$VERIFY_BIN")/.pyo-verify.new
    install -m 755 "$dir/pyo-verify" "$new" 2>/dev/null || rc=$?
    rm -rf "$dir"
  fi
  [ "$rc" != 0 ] || verify_step "$new" install --url "$url" ${channel[@]+"${channel[@]}"} || rc=$?
  [ "$new" = "$VERIFY_BIN" ] || rm -f "$new"
  if [ "$rc" = 0 ]; then
    progress_end ok "Set up automatic updates"
  else
    progress_end wait "Automatic updates couldn't be set up on this server."
  fi
  return 0
}
# Removes the verifier from this server. When it can't do that itself (a
# file that doesn't run here), its timer and files go anyway.
verify_remove() {
  "$VERIFY_BIN" uninstall >/dev/null 2>&1 9>&- && return 0
  systemctl disable --now pyo-verify.timer >/dev/null 2>&1 9>&- || true
  rm -f "$VERIFY_UNITS/pyo-verify.timer" "$VERIFY_UNITS/pyo-verify.service" "$VERIFY_BIN" "$VERIFY_BIN.previous" "$VERIFY_BIN.new" "$(dirname "$VERIFY_BIN")/.pyo-verify.new"
  systemctl daemon-reload >/dev/null 2>&1 9>&- || true
  [ ! -e "$VERIFY_BIN" ]
}
auto_update_off() {
  [ -e "$VERIFY_BIN" ] || return 0
  if verify_remove; then
    progress_paint 32 ✓
    echo " Turned off automatic updates"
  else
    dot_line yellow "Automatic updates couldn't be turned off on this server."
  fi
  return 0
}
# <<< pyo:auto-update
# >>> pyo:install-url (connect.sh only; join.rs tests it)
# Where this server gets Pyo's install files (the agent's manifest, the
# gateway's CRDs) and where its verifier looks for releases: INSTALL_URL.
# On a Pyo whose agents reach it at an address only they may use (Pyo
# Cloud's), Pyo's commands name its public address with --install-url. It
# is kept for later runs without it (INSTALL_URL_FILE). Elsewhere the
# agent's address serves both. The server Pyo runs on keeps nothing.
INSTALL_URL_FILE=/etc/pyo/install-url
FROM_AGENT=""
agent_secret() { k3s kubectl -n pyo-system get secret pyo-agent -o "jsonpath={.data.$1}" 2>/dev/null | base64 -d 2>/dev/null || true; }
# A refresh: Pyo's address and the token from the agent already here.
agent_settings() {
  FROM_AGENT=1
  API=$(agent_secret PYO_API_URL); API="${API%/}"; TOKEN=$(agent_secret PYO_AGENT_TOKEN)
  # The cluster Pyo runs on: its agent reaches Pyo inside the cluster, this
  # machine at Pyo's local address.
  case "$API" in http://*.svc:* | http://*.svc.cluster.local:*) API=http://127.0.0.1:8000 ;; esac
  [ -n "$INSTALL_URL" ] || [ -e "$OWN_SERVER" ] || INSTALL_URL=$(install_url_kept)
}
# The address an earlier run kept, when it is one.
install_url_kept() {
  local u
  u=$(head -n1 "$INSTALL_URL_FILE" 2>/dev/null || true)
  case "$u" in https://?* | http://?*) echo "${u%/}" ;; esac
}
# Keeps --install-url for later runs. A connect without it (--api) has one
# address for both, so a kept one goes.
install_url_keep() {
  [ ! -e "$OWN_SERVER" ] || return 0
  if [ -n "$INSTALL_URL" ]; then
    { mkdir -p "$(dirname "$INSTALL_URL_FILE")" && echo "$INSTALL_URL" > "$INSTALL_URL_FILE"; } 2>/dev/null || true
  elif [ -z "$FROM_AGENT" ]; then
    rm -f "$INSTALL_URL_FILE" 2>/dev/null || true
  fi
}
# An http:// address with its tailnet name as tailnet_hostport has it.
tailnet_url() {
  case "$1" in
    http://*)
      local hostport=${1#http://}
      hostport=${hostport%%/*}
      echo "http://$(tailnet_hostport "$hostport")${1#"http://$hostport"}"
      ;;
    *) echo "$1" ;;
  esac
}
# <<< pyo:install-url

# ── connect.sh --uninstall ──────────────────────────────────────────────────
# Deletes only what Pyo made, found by its names and labels: the agent
# first (so nothing is put back), then projects and databases (while their
# operators still run, for their finalizers), then the system bundles, Pyo
# Gateway, cert-manager (if Pyo installed it and nothing else uses it), and
# the agent's roles. Other namespaces, CRDs and Gateway API stay. With
# --keep-data, the volumes in the namespaces it removes (Pyo's own database,
# registry and backup store, with --with-pyo) are set to Retain first: their
# data and PersistentVolumes stay, listed at the end.
uninstall() {
  local KC
  if [ -z "${KUBECONFIG:-}" ] && command -v k3s >/dev/null; then
    [ "$(id -u)" = 0 ] || { echo "Run it as root (sudo)." >&2; exit 1; }
    KC=(k3s kubectl)
  elif command -v kubectl >/dev/null; then
    KC=(kubectl)
  else
    echo "There's no k3s or kubectl on this machine. Run this on the cluster's first server." >&2
    exit 1
  fi
  kc() { "${KC[@]}" "$@"; }
  # Changes go through here: --dry-run only prints them.
  act() {
    if [ -n "$DRY_RUN" ]; then echo "  would run: kubectl $*" >&4; else kc "$@"; fi
  }
  lines() { kc get "$@" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null || true; }
  has() { kc get "$@" >/dev/null 2>&1; }
  ours() { [ "$(kc get "$@" -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}' 2>/dev/null || true)" = pyo ]; }
  # Waits until `kc get <args>` lists nothing (up to $1 seconds).
  gone() {
    local secs=$1 i
    shift
    [ -z "$DRY_RUN" ] || return 0
    for ((i = 0; i < secs; i += 3)); do
      [ -z "$(kc get "$@" -o name 2>/dev/null || true)" ] && return 0
      sleep 3
    done
    return 1
  }
  del_ns() { # namespaces, waited for
    local ns
    for ns in "$@"; do act delete namespace "$ns" --ignore-not-found --wait=false >/dev/null; done
    for ns in "$@"; do
      gone 600 namespace "$ns" --ignore-not-found || {
        echo "Namespace $ns is still being deleted after 10 minutes. Look at: kubectl get all -n $ns" >&2
        echo "Run this again once it's gone. Nothing after it was removed yet." >&2
        exit 1
      }
    done
  }

  exec 4>&1
  has namespace kube-system || { echo "Can't reach the cluster with ${KC[*]}." >&2; exit 1; }
  local server
  server=$(kc config view --minify -o jsonpath='{.clusters[0].cluster.server}' 2>/dev/null || true)

  local pyo_here=""
  has -n pyo-system deployment pyo && pyo_here=1
  if [ -n "$pyo_here" ] && [ -z "$WITH_PYO" ]; then
    echo "This cluster runs Pyo itself. To remove Pyo from this machine, run:" >&2
    echo "  curl -fsSL https://pyo.dev/install.sh | bash -s -- --uninstall" >&2
    exit 1
  fi

  # What's here.
  local projects=() bundles=() ns id line
  mapfile -t projects < <(lines namespace -l pyo.dev/managed=true | grep -vx pyo-system || true)
  while IFS= read -r line; do
    id=${line%% *} ns=${line#* }
    [ -n "$id" ] && [ -n "$ns" ] || continue
    case "$ns" in default | kube-* | pyo-system | pyo-gateway | cert-manager) continue ;; esac
    bundles+=("$id=$ns")
  done < <(kc get namespace -l pyo.dev/bundle -o jsonpath='{range .items[*]}{.metadata.labels.pyo\.dev/bundle} {.metadata.name}{"\n"}{end}' 2>/dev/null || true)
  local dbs=0
  has crd databases.pyo.dev && dbs=$(kc get databases.pyo.dev -A --no-headers 2>/dev/null | wc -l)
  local keep_db_system=""
  if [ -n "$KEEP_DATA" ]; then
    for line in "${bundles[@]}"; do [ "${line%%=*}" = databases ] && keep_db_system=1; done
  fi
  local gateway_chart="" cm_chart=""
  ours -n kube-system helmchart.helm.cattle.io envoy-gateway && gateway_chart=1
  ours -n kube-system helmchart.helm.cattle.io cert-manager && cm_chart=1
  # cert-manager stays when something else uses it: certificates or issuers
  # outside the namespaces this removes.
  local cm_users="" removed_ns=" pyo-system pyo-gateway cert-manager "
  for line in "${bundles[@]}"; do
    [ -n "$keep_db_system" ] && [ "${line%%=*}" = databases ] && continue
    removed_ns+="${line#*=} "
  done
  [ -n "$KEEP_DATA" ] || removed_ns+="${projects[*]} "
  if [ -n "$cm_chart" ]; then
    cm_users=$(kc get certificates.cert-manager.io,issuers.cert-manager.io -A -o jsonpath='{range .items[*]}{.metadata.namespace}{"\n"}{end}' 2>/dev/null | sort -u | while read -r ns; do
      case "$removed_ns" in *" $ns "*) ;; *) echo "$ns" ;; esac
    done | xargs)
  fi
  local remove_cm=""
  [ -n "$cm_chart" ] && [ -z "$keep_db_system" ] && [ -z "$cm_users" ] && remove_cm=1
  # Keeping data: the volumes claimed in namespaces this removes, kept on
  # disk (Retain) as "name|namespace/claim|policy|path|server".
  local retain=() pv claim_ns claim policy path node
  if [ -n "$KEEP_DATA" ]; then
    while IFS='|' read -r pv claim_ns claim policy path node; do
      [ -n "$pv" ] && [ -n "$claim_ns" ] || continue
      case "$removed_ns" in *" $claim_ns "*) ;; *) continue ;; esac
      retain+=("$pv|$claim_ns/$claim|$policy|$path|$node")
    done < <(kc get pv -o jsonpath='{range .items[*]}{.metadata.name}|{.spec.claimRef.namespace}|{.spec.claimRef.name}|{.spec.persistentVolumeReclaimPolicy}|{.spec.local.path}{.spec.hostPath.path}|{.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].values[0]}{"\n"}{end}' 2>/dev/null || true)
  fi

  # Automatic updates on this server (the verifier), when it's this server
  # being cleaned up.
  local verify_here=""
  [ "${KC[0]}" != k3s ] || [ ! -e "$VERIFY_BIN" ] || verify_here=1

  echo "This removes Pyo from the cluster at ${server:-?}:"
  echo "  - the Pyo agent, its roles and admission policy (namespace pyo-system)"
  [ -z "$verify_here" ] || echo "  - automatic updates on this server"
  [ -z "$pyo_here" ] || echo "  - Pyo itself, with its own database, registry and backup store (pyo-system)"
  has namespace pyo-gateway && echo "  - Pyo Gateway (namespace pyo-gateway${gateway_chart:+, Envoy Gateway})"
  for line in "${bundles[@]}"; do
    [ -n "$keep_db_system" ] && [ "${line%%=*}" = databases ] && continue
    echo "  - the ${line%%=*} system (namespace ${line#*=})"
  done
  [ -z "$remove_cm" ] || echo "  - cert-manager (namespace cert-manager), which Pyo installed"
  if [ -z "$KEEP_DATA" ] && [ "${#projects[@]}" -gt 0 ]; then
    echo "  - ALL PROJECT DATA: ${#projects[@]} namespace(s) with their services, volumes and $dbs database(s):"
    echo "      ${projects[*]}"
  fi
  local kept=()
  if [ -n "$KEEP_DATA" ] && [ "${#projects[@]}" -gt 0 ]; then
    kept+=("project namespaces, with their services, volumes and $dbs database(s): ${projects[*]}")
  fi
  if [ -n "$keep_db_system" ]; then kept+=("the databases system that runs them (pyo-databases), and cert-manager"); fi
  if [ -n "$cm_users" ] && [ -z "$keep_db_system" ]; then kept+=("cert-manager, which these namespaces use: $cm_users"); fi
  if [ "${#kept[@]}" -gt 0 ]; then
    echo "Kept:"
    printf '  - %s\n' "${kept[@]}"
  fi
  if [ "${#retain[@]}" -gt 0 ]; then
    echo "Kept on the servers' disks, with their volumes (set to Retain), the data of:"
    for line in "${retain[@]}"; do
      IFS='|' read -r pv claim policy path node <<<"$line"
      echo "  - $claim: PersistentVolume $pv${path:+, $path}${node:+ on $node}"
    done
  fi
  if [ -z "$KEEP_DATA" ] && [ "${#projects[@]}" -gt 0 ]; then
    echo
    echo "Deleted data can't be brought back. Back it up first: https://pyo.dev/docs/uninstall"
  fi
  echo
  if [ -n "$DRY_RUN" ]; then
    echo "Dry run: nothing is changed."
  elif [ -z "$YES" ]; then
    local answer=""
    if [ -r /dev/tty ] && { exec 3</dev/tty; } 2>/dev/null; then
      read -r -p "Type yes to remove it: " answer <&3 || true
      exec 3<&-
    else
      echo "Run again with --yes to remove it without asking." >&2
      exit 1
    fi
    [ "$answer" = yes ] || { echo "Nothing was changed."; exit 1; }
  fi

  # 0. Kept volumes: Retain, so deleting their claims keeps their data.
  for line in "${retain[@]}"; do
    IFS='|' read -r pv claim policy path node <<<"$line"
    [ "$policy" = Retain ] || act patch pv "$pv" --type=merge -p '{"spec":{"persistentVolumeReclaimPolicy":"Retain"}}' >/dev/null
  done

  # 1. Automatic updates on this server, between the verifier's runs. Then
  # the agent, so it stops putting things back; its admission policy.
  if [ -n "$verify_here" ]; then
    if [ -n "$DRY_RUN" ]; then
      echo "  would remove automatic updates from this server" >&4
    else
      echo "Removing automatic updates…"
      connect_lock
      verify_remove || echo "Automatic updates couldn't be removed from this server." >&2
    fi
  fi
  [ "${KC[0]}" != k3s ] || [ -n "$DRY_RUN" ] || rm -f "$AUTO_UPDATE_FILE" "$INSTALL_URL_FILE"
  echo "Stopping the Pyo agent…"
  act -n pyo-system delete deployment pyo-agent --ignore-not-found --wait=true --timeout=120s >/dev/null
  act delete validatingadmissionpolicybinding,validatingadmissionpolicy -l app.kubernetes.io/part-of=pyo-agent --ignore-not-found >/dev/null 2>&1 || true

  # 2. Projects and databases, while the operators run for their finalizers;
  # or, keeping them, only the agent's bindings in them.
  if [ -z "$KEEP_DATA" ]; then
    if [ "$dbs" -gt 0 ]; then
      echo "Deleting $dbs database(s)…"
      act delete databases.pyo.dev --all -A --wait=false >/dev/null
      gone 600 databases.pyo.dev -A || echo "Some databases are still being deleted. Carrying on with their namespaces." >&2
    fi
    if [ "${#projects[@]}" -gt 0 ]; then
      echo "Deleting ${#projects[@]} project namespace(s)…"
      del_ns "${projects[@]}"
    fi
  else
    for ns in "${projects[@]}"; do
      if [ "$(kc -n "$ns" get rolebinding pyo-agent -o jsonpath='{.roleRef.name}' 2>/dev/null || true)" = pyo-agent-write ]; then
        act -n "$ns" delete rolebinding pyo-agent --ignore-not-found >/dev/null
      fi
    done
  fi

  # 3. System bundles: their webhooks, bindings, namespace, then CRDs and
  # cluster roles (all labelled pyo.dev/bundle=<id>).
  for line in "${bundles[@]}"; do
    id=${line%%=*} ns=${line#*=}
    [ -n "$keep_db_system" ] && [ "$id" = databases ] && continue
    echo "Removing the $id system ($ns)…"
    act delete validatingwebhookconfiguration,mutatingwebhookconfiguration -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null
    act delete validatingadmissionpolicybinding,validatingadmissionpolicy -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null 2>&1 || true
    act delete rolebinding -A -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null
    del_ns "$ns"
    act delete crd -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null
    act delete clusterrolebinding,clusterrole -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null
  done

  # 4. Pyo Gateway: its routes and Gateways (Envoy Gateway tears down their
  # proxies), its class, then Envoy Gateway itself and its CRDs.
  if has namespace pyo-gateway; then
    echo "Removing Pyo Gateway…"
    has crd gateways.gateway.networking.k8s.io && act -n pyo-gateway delete gateways.gateway.networking.k8s.io,httproutes.gateway.networking.k8s.io --all --ignore-not-found --wait=false >/dev/null
    has crd tcproutes.gateway.networking.k8s.io && act -n pyo-gateway delete tcproutes.gateway.networking.k8s.io --all --ignore-not-found --wait=false >/dev/null
    has crd udproutes.gateway.networking.k8s.io && act -n pyo-gateway delete udproutes.gateway.networking.k8s.io --all --ignore-not-found --wait=false >/dev/null
    has crd gateways.gateway.networking.k8s.io && { gone 120 -n pyo-gateway gateways.gateway.networking.k8s.io || true; }
  fi
  if ours gatewayclass.gateway.networking.k8s.io pyo; then
    act delete gatewayclass.gateway.networking.k8s.io pyo --ignore-not-found --wait=false >/dev/null
    gone 60 gatewayclass.gateway.networking.k8s.io pyo --ignore-not-found || act patch gatewayclass.gateway.networking.k8s.io pyo --type=merge -p '{"metadata":{"finalizers":null}}' >/dev/null 2>&1 || true
  fi
  if [ -n "$gateway_chart" ]; then
    act -n kube-system delete helmchart.helm.cattle.io envoy-gateway --ignore-not-found --wait=false >/dev/null
    gone 300 -n kube-system helmchart.helm.cattle.io envoy-gateway --ignore-not-found || echo "Envoy Gateway's chart is still being removed. Carrying on." >&2
  fi
  has namespace pyo-gateway && del_ns pyo-gateway
  # Its CRDs (applied before its chart), unless another Envoy Gateway runs
  # here. The Gateway API stays, k3s's or the copy Pyo applied.
  if [ -n "$gateway_chart" ] && [ -z "$(kc get deployment -A -l control-plane=envoy-gateway -o jsonpath='{range .items[*]}{.metadata.namespace}{"\n"}{end}' 2>/dev/null | grep -vx pyo-gateway || true)" ]; then
    local crds=()
    mapfile -t crds < <(lines crd | grep '\.gateway\.envoyproxy\.io$' || true)
    [ "${#crds[@]}" -eq 0 ] || act delete crd "${crds[@]}" --ignore-not-found >/dev/null
  fi

  # 5. cert-manager: Pyo's issuers; cert-manager itself if Pyo installed it
  # and nothing else uses it.
  act delete clusterissuer.cert-manager.io -l app.kubernetes.io/managed-by=pyo --ignore-not-found >/dev/null 2>&1 || true
  if [ -n "$remove_cm" ]; then
    echo "Removing cert-manager…"
    act -n kube-system delete helmchart.helm.cattle.io cert-manager --ignore-not-found --wait=false >/dev/null
    gone 300 -n kube-system helmchart.helm.cattle.io cert-manager --ignore-not-found || echo "cert-manager's chart is still being removed. Carrying on." >&2
    has namespace cert-manager && del_ns cert-manager
    act delete crd -l app.kubernetes.io/instance=cert-manager,app.kubernetes.io/name=cert-manager --ignore-not-found >/dev/null
  fi

  # 6. kube-system: join tokens, the GPU device plugin, the CoreDNS budget,
  # the agent's role there. Then its cluster roles, and pyo-system.
  echo "Removing the agent's roles…"
  act -n kube-system delete secret,daemonset,poddisruptionbudget,rolebinding,role -l app.kubernetes.io/managed-by=pyo --ignore-not-found >/dev/null
  act delete clusterrolebinding,clusterrole -l app.kubernetes.io/part-of=pyo-agent --ignore-not-found >/dev/null
  del_ns pyo-system

  # 7. Volumes left Released by what was deleted (a Retain policy keeps
  # their data on the server's disk; the object goes), but those kept.
  local phase kept_pv=()
  while read -r pv phase claim policy path; do
    [ "$phase" = Released ] || continue
    case "$removed_ns" in *" $claim "*) ;; *) continue ;; esac
    [ "${#retain[@]}" -eq 0 ] || case " ${retain[*]%%|*} " in *" $pv "*) continue ;; esac
    act delete pv "$pv" --ignore-not-found >/dev/null
    [ "$policy" != Retain ] || kept_pv+=("$pv${path:+ ($path)}")
  done < <(kc get pv -o jsonpath='{range .items[*]}{.metadata.name} {.status.phase} {.spec.claimRef.namespace} {.spec.persistentVolumeReclaimPolicy} {.spec.local.path}{.spec.hostPath.path}{"\n"}{end}' 2>/dev/null || true)

  echo
  if [ -n "$DRY_RUN" ]; then
    echo "Dry run done: nothing was changed."
  else
    echo "Removed Pyo from this cluster. The cluster and everything else in it stay."
  fi
  if [ "${#kept_pv[@]}" -gt 0 ]; then
    echo "These volumes kept their data on the server's disk (Retain). Delete it there when you're done:"
    printf '  %s\n' "${kept_pv[@]}"
  fi
  if [ -n "$KEEP_DATA" ] && [ "${#projects[@]}" -gt 0 ]; then
    echo "Kept: ${projects[*]}. To delete one later: kubectl delete namespace <name>"
  fi
  if [ "${#retain[@]}" -gt 0 ] && [ -z "$DRY_RUN" ]; then
    local names=() k=kubectl
    [ "${KC[0]}" != k3s ] || k="sudo k3s kubectl"
    echo "These volumes' data was kept on the servers' disks:"
    for line in "${retain[@]}"; do
      IFS='|' read -r pv claim policy path node <<<"$line"
      names+=("$pv")
      echo "  $claim: PersistentVolume $pv${path:+, $path}${node:+ on $node}"
    done
    echo "To delete it later: $k delete pv ${names[*]}"
    echo "then, on each server, remove its directory: sudo rm -rf <path above>"
  fi
}
if [ -n "$UNINSTALL" ]; then
  uninstall
  exit 0
fi
[ -n "$TAILNET_ONLY" ] || connect_lock
# Without them, on a cluster that's already connected: bring its agent's
# install up to date with Pyo (Pyo shows this command when it's older).
if [ -z "$TAILNET_ONLY" ] && [ -z "$API" ] && [ -z "$TOKEN" ] && command -v k3s >/dev/null; then
  agent_settings
  [ -z "$API" ] || echo "Updating this cluster's Pyo agent."
fi
[ -n "$TAILNET_ONLY" ] || { [ -n "$API" ] && [ -n "$TOKEN" ]; } || { echo "Usage: connect.sh --api URL --token TOKEN [--tailscale-key KEY] [--registry HOST:PORT --registry-token TOKEN | --local-registry]" >&2; exit 2; }
[ -z "$REGISTRY" ] || [ -n "$REGISTRY_TOKEN" ] || { echo "--registry needs --registry-token: copy the full command from Pyo." >&2; exit 2; }
case "$REGISTRY_TOKEN" in *[!0-9a-fA-F]*) echo "That registry token doesn't look like one from Pyo." >&2; exit 2 ;; esac
# The agent's token and its commands' secrets travel to --api: plain http
# only to a private address (this machine, a private network, the tailnet,
# the cluster), never across the internet.
api_ok() {
  case "$1" in
    https://?*) return 0 ;;
    http://*) ;;
    *) return 1 ;;
  esac
  local h="${1#http://}"
  h="${h%%[/?#]*}"; h="${h##*@}"
  case "$h" in
    \[*) h="${h#[}"; h="${h%%]*}" ;;
    *) h="${h%%:*}" ;;
  esac
  h=$(printf '%s' "$h" | tr 'A-Z' 'a-z'); h="${h%.}"
  if [[ "$h" =~ ^[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$ ]]; then
    case "$h" in
      127.* | 10.* | 192.168.*) return 0 ;;
      172.1[6-9].* | 172.2[0-9].* | 172.3[01].*) return 0 ;;
      100.6[4-9].* | 100.[7-9][0-9].* | 100.1[01][0-9].* | 100.12[0-7].*) return 0 ;;
    esac
    return 1
  fi
  case "$h" in
    localhost | *.localhost | *.svc | *.svc.* | *.cluster.local | *.ts.net | ::1 | fc*:* | fd*:*) return 0 ;;
  esac
  return 1
}
[ -n "$TAILNET_ONLY" ] || api_ok "$API" || { echo "Pyo's address must be https:// ($API isn't on a private network or the tailnet)." >&2; exit 2; }
[ -n "$TAILNET_ONLY" ] || [ -z "$INSTALL_URL" ] || api_ok "$INSTALL_URL" || { echo "Pyo's address must be https:// ($INSTALL_URL isn't on a private network or the tailnet)." >&2; exit 2; }
[ "$(id -u)" = 0 ] || { echo "Run it as root (sudo)." >&2; exit 1; }
[ "$(uname -s)" = Linux ] || { echo "Pyo clusters run on Linux, and this is $(uname -s)." >&2; exit 1; }
case "$(uname -m)" in
  x86_64 | amd64) ;;
  *) echo "Pyo's agent is built for x86-64 (amd64), and this machine is $(uname -m)." >&2; exit 1 ;;
esac
command -v curl >/dev/null || { echo "curl is missing. Install it and run this again." >&2; exit 1; }
[ -n "$TAILNET_ONLY" ] || case "$TOKEN" in *[!0-9a-fA-F]* | "") echo "That token doesn't look like one from Pyo (letters a-f and digits only)." >&2; exit 2 ;; esac

# >>> pyo:k3s-pin (deploy/k3s.env)
PYO_K3S_VERSION=v1.37.2+k3s1
PYO_K3S_INSTALLER_SHA256=ed01f89fd977bf20ac1516bbebf8370bf3ddbaa55dac8aba610956a4c78cc00b
PYO_K3S_SHA256_AMD64=265b3ee6a3198ef14033c63ef46cbb32370d83795199fb6d1b01f21575eddbda
PYO_K3S_SHA256_ARM64=a779ccf1e5e2e33a62f5a29c2f1c0bbf99431e100579d4c10f0aa0742a6aa84b
# <<< pyo:k3s-pin

# >>> pyo:k3s (the same in deploy/install.sh, connect.sh, join.sh, scripts/setup-host.sh and scripts/join-node.sh)
# k3s, checked before anything is installed: k3s's own installer at the
# tag of the pinned version (PYO_K3S_VERSION) against its SHA-256, and the
# binary against the pinned SHA-256 for this architecture, or, for another
# version (a cluster that runs one), against that GitHub release's checksum
# list. k3s_fetch DIR VERSION downloads and checks; k3s_install DIR ARGS...
# puts the binary in /usr/local/bin and runs the installer with ARGS
# (INSTALL_K3S_EXEC, K3S_URL and K3S_TOKEN pass through).
k3s_fail() {
  echo "Error: $*" >&2
  exit 1
}
# Four tries, as a download can fail on a bad connection. Not curl's
# --retry-all-errors, which older curl (RHEL 8, Ubuntu 20.04) lacks.
k3s_get() {
  local i
  for i in 1 2 3 4; do
    curl -fsSL --proto '=https' --max-time 600 -o "$2" "$1" && return 0
    [ "$i" = 4 ] || sleep 2
  done
  k3s_fail "Couldn't download $1."
}
k3s_sha_ok() { [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ]; }
k3s_fetch() {
  local dir=$1 version=$2 arch bin want rel
  case "$(uname -m)" in
    x86_64 | amd64) arch=amd64 bin=k3s want=${PYO_K3S_SHA256_AMD64:-} ;;
    aarch64 | arm64) arch=arm64 bin=k3s-arm64 want=${PYO_K3S_SHA256_ARM64:-} ;;
    *) k3s_fail "There's no k3s for $(uname -m)." ;;
  esac
  [[ ${PYO_K3S_VERSION:-} =~ ^v[0-9]+\.[0-9]+\.[0-9]+\+k3s[0-9]+$ ]] && [[ ${PYO_K3S_INSTALLER_SHA256:-} =~ ^[0-9a-f]{64}$ ]] ||
    k3s_fail "This script doesn't say which k3s to install."
  [[ $version =~ ^v[0-9]+\.[0-9]+\.[0-9]+\+k3s[0-9]+$ ]] || k3s_fail "\"$version\" isn't a k3s version, like $PYO_K3S_VERSION."
  rel="https://github.com/k3s-io/k3s/releases/download/${version//+/%2B}"
  mkdir -p "$dir"
  echo "Downloading k3s $version and checking it…"
  k3s_get "https://raw.githubusercontent.com/k3s-io/k3s/${PYO_K3S_VERSION//+/%2B}/install.sh" "$dir/install.sh"
  k3s_sha_ok "$dir/install.sh" "$PYO_K3S_INSTALLER_SHA256" ||
    k3s_fail "The downloaded k3s installer doesn't match the expected checksum, so it won't run."
  if [ "$version" != "$PYO_K3S_VERSION" ]; then
    k3s_get "$rel/sha256sum-$arch.txt" "$dir/sha256sum.txt"
    want=$(awk -v f="$bin" '$2 == f { print $1 }' "$dir/sha256sum.txt")
  fi
  [[ $want =~ ^[0-9a-f]{64}$ ]] || k3s_fail "There's no checksum for k3s $version ($bin)."
  k3s_get "$rel/$bin" "$dir/k3s"
  k3s_sha_ok "$dir/k3s" "$want" ||
    k3s_fail "The downloaded k3s binary doesn't match the expected checksum, so it won't be installed."
  chmod 755 "$dir/k3s"
}
k3s_install() {
  local dir=$1
  shift
  [ -x "$dir/k3s" ] && [ -f "$dir/install.sh" ] || k3s_fail "k3s wasn't downloaded."
  install -m 755 "$dir/k3s" /usr/local/bin/.k3s.new
  mv -f /usr/local/bin/.k3s.new /usr/local/bin/k3s
  INSTALL_K3S_SKIP_DOWNLOAD=binary INSTALL_K3S_BIN_DIR=/usr/local/bin sh "$dir/install.sh" "$@"
}
# <<< pyo:k3s
# >>> pyo:small-machine (the same in deploy/install.sh, connect.sh, join.sh, scripts/setup-host.sh and scripts/join-node.sh)
# Run before k3s is installed, or on a k3s that's already installed with
# k3s_memory_refresh. What follows this machine's memory, worked out again
# at every boot by /usr/local/lib/pyo-memory.sh, so a machine given more
# or less memory gets the matching settings when it restarts.
# Under 3.5 GB of memory: k3s gets a soft memory limit, GOMEMLIMIT
# (gomemlimit in the helper), so its Go runtime collects garbage sooner
# instead of growing. Under 2.75 GB, also compressed swap in memory
# (zram, half the memory, pyo-zram.service) for what runs outside
# containers, k3s itself above all (containers don't swap), unless the
# machine has swap of its own. From 3.5 GB, neither. At any size, kubelet
# sets aside memory for k3s and the system (kube-reserved and
# system-reserved, in a k3s config drop-in), so the scheduler only hands
# containers what's left: 256 MiB under 1.5 GB, 512 MiB under 3.5 GB,
# 768 MiB under 8 GB, 1 GiB from there. The same drop-in holds when
# kubelet frees disk space (disk_settings in the helper).
# K3S_UNIT: k3s's systemd unit, k3s (a server) unless the script sets
# k3s-agent first (a server joined to a cluster).
K3S_UNIT=${K3S_UNIT:-k3s}
K3S_MEMORY_CONFIG=/etc/rancher/k3s/config.yaml.d/50-pyo-memory.yaml
K3S_MEMORY_CHANGED=""
small_machine() {
  local env before
  # What k3s runs with now: kubelet's reservations and its GOMEMLIMIT.
  before=$(cat "$K3S_MEMORY_CONFIG" /run/pyo-k3s.env 2>/dev/null || true)
  mkdir -p "/etc/systemd/system/$K3S_UNIT.service.d"
  cat > /usr/local/lib/pyo-memory.sh <<'MEMORY'
#!/bin/sh
# Managed by Pyo: k3s's GOMEMLIMIT, kubelet's settings and compressed
# swap (zram), from this machine's memory at every boot.
mem=$(awk '/^MemTotal:/ { print int($2 / 1024) }' "${PYO_MEMINFO:-/proc/meminfo}")
# k3s's soft memory limit in MiB: 40% of the memory, at least 768 MiB
# (k3s's own heap is 330 to 400 MB: a lower limit keeps its garbage
# collector running all the time).
gomemlimit() {
  limit=$((mem * 2 / 5))
  [ "$limit" -ge 768 ] || limit=768
  echo "$limit"
}
# What kubelet sets aside in MiB, for k3s (kube-reserved: its one process
# holds 500 to 600 MB on a small machine) and for the system
# (system-reserved: sshd, tailscaled, journald, …).
reserved() {
  if [ "$mem" -lt 1536 ]; then
    echo "192 64"
  elif [ "$mem" -lt 3584 ]; then
    echo "384 128"
  elif [ "$mem" -lt 8192 ]; then
    echo "512 256"
  else
    echo "768 256"
  fi
}
# When kubelet frees disk space, at any size. Under 5% free it deletes
# unused images, then evicts pods, and the server counts as short of disk
# (DiskPressure) until it has 1 GiB more than that. k3s's own 10% more
# kept a server whose disk was filled by something else short of disk
# for hours, evicting nearly every pod for space they didn't hold.
# Unused images go from 85% used down to 75%, well before any of that.
# eviction-hard replaces k3s's whole list, which has these two only: no
# memory or inode limits (the reservations above leave the memory room).
disk_settings() {
  printf '  - "%s"\n' 'eviction-hard=imagefs.available<5%,nodefs.available<5%' \
    'eviction-minimum-reclaim=imagefs.available=1Gi,nodefs.available=1Gi' \
    'image-gc-high-threshold=85' 'image-gc-low-threshold=75'
}
case "$1" in
  k3s-env)
    # For k3s's EnvironmentFile: GOMEMLIMIT under 3.5 GB, else nothing.
    if [ "${mem:-0}" -gt 0 ] && [ "$mem" -lt 3584 ]; then
      echo "GOMEMLIMIT=$(gomemlimit)MiB"
    fi
    ;;
  k3s-config)
    # k3s's config drop-in ($2): kubelet's reservations (none when the
    # memory can't be read) and its disk settings.
    [ -n "$2" ] || exit 2
    out=$2
    mkdir -p "$(dirname "$out")"
    {
      echo "# Managed by Pyo: kubelet's settings, from this machine's memory at each start."
      echo 'kubelet-arg+:'
      if [ "${mem:-0}" -gt 0 ]; then
        set -- $(reserved)
        printf '  - "%s"\n' "kube-reserved=memory=$1Mi" "system-reserved=memory=$2Mi"
      fi
      disk_settings
    } > "$out.new" && mv -f "$out.new" "$out"
    ;;
  zram-start)
    [ "${mem:-0}" -gt 0 ] && [ "$mem" -lt 2816 ] || exit 0
    [ -z "$(swapon --show=NAME --noheadings 2>/dev/null)" ] || exit 0
    command -v zramctl >/dev/null && modprobe zram 2>/dev/null || exit 0
    d=$(zramctl --find --size "$((mem / 2))MiB") && mkswap "$d" >/dev/null && swapon -p 100 "$d" && echo "$d" > /run/pyo-zram
    ;;
  zram-stop)
    [ -f /run/pyo-zram ] || exit 0
    d=$(cat /run/pyo-zram)
    swapoff "$d"
    for i in 1 2 3 4 5; do
      zramctl --reset "$d" 2>/dev/null && break
      sleep 1
    done
    rm -f /run/pyo-zram
    ;;
esac
MEMORY
  chmod 755 /usr/local/lib/pyo-memory.sh
  printf '%s\n' '# Managed by Pyo: GOMEMLIMIT on a small machine and what kubelet sets aside, worked out at each start.' '[Service]' \
    "ExecStartPre=-/bin/sh -c '/usr/local/lib/pyo-memory.sh k3s-env > /run/pyo-k3s.env'" \
    "ExecStartPre=-/usr/local/lib/pyo-memory.sh k3s-config $K3S_MEMORY_CONFIG" \
    'EnvironmentFile=-/run/pyo-k3s.env' > "/etc/systemd/system/$K3S_UNIT.service.d/pyo-memory.conf"
  printf '%s\n' '[Unit]' 'Description=Pyo: compressed swap in memory (zram) on a small machine' "Before=$K3S_UNIT.service" \
    '[Service]' 'Type=oneshot' 'RemainAfterExit=yes' \
    'ExecStart=/usr/local/lib/pyo-memory.sh zram-start' 'ExecStop=/usr/local/lib/pyo-memory.sh zram-stop' \
    '[Install]' 'WantedBy=multi-user.target' > /etc/systemd/system/pyo-zram.service
  systemctl daemon-reload
  systemctl enable --now pyo-zram.service >/dev/null 2>&1 || echo "Couldn't set up compressed swap. Going on without it." >&2
  env=$(/usr/local/lib/pyo-memory.sh k3s-env)
  /usr/local/lib/pyo-memory.sh k3s-config "$K3S_MEMORY_CONFIG" || true
  [ "$before" = "$(cat "$K3S_MEMORY_CONFIG" 2>/dev/null; [ -z "$env" ] || echo "$env")" ] || K3S_MEMORY_CHANGED=1
}
# The same on a k3s that's already installed ($K3S_UNIT.service). k3s uses what
# changed once it restarts: it restarts here, unless $1 is "later" (the
# caller restarts it, when K3S_MEMORY_CHANGED is set). Containers keep
# running while k3s restarts.
k3s_memory_refresh() {
  systemctl cat "$K3S_UNIT.service" >/dev/null 2>&1 || return 0
  K3S_MEMORY_CHANGED=""
  small_machine
  [ -n "$K3S_MEMORY_CHANGED" ] || return 0
  echo "k3s restarts briefly. Services keep running."
  [ "${1:-}" = later ] || systemctl restart "$K3S_UNIT"
}
# <<< pyo:small-machine

# >>> pyo:repos (the same in connect.sh, join.sh, scripts/join-node.sh and deploy/sandbox.sh)
# Packages come from their vendors' repositories, whose signing keys are
# checked by fingerprint here: Tailscale's and NVIDIA's.
TAILSCALE_KEY=2596A99EAAB33821893C0A79458CA832957F5868
# shellcheck disable=SC2034 # for the GPU setup, which not every script has
NVIDIA_KEY=C95B321B61E88C1809C4F759DDCAE044F796ECB0
os_field() { sed -n "s/^$1=//p" /etc/os-release 2>/dev/null | tr -d '"' | head -n1; }
# signing_key URL FINGERPRINT FILE [armor]: the key with that fingerprint
# from URL into FILE (only that key), or an error.
signing_key() {
  local url=$1 fpr=$2 out=$3 g ok="" pm got="" i
  if ! command -v gpg >/dev/null; then
    if command -v apt-get >/dev/null; then
      apt-get update -qq && apt-get install -y -qq gnupg >/dev/null
    elif pm=$(command -v dnf || command -v yum); then
      "$pm" install -y -q gnupg2 >/dev/null
    fi || true
    command -v gpg >/dev/null || { echo "gpg is missing, so the signing key at $url can't be checked. Install gnupg and run this again." >&2; return 1; }
  fi
  g=$(mktemp -d)
  for i in 1 2 3 4; do
    curl -fsSL --proto '=https' --max-time 60 -o "$g/key" "$url" && got=1 && break
    [ "$i" = 4 ] || sleep 2
  done
  if [ -z "$got" ]; then
    echo "Couldn't download the signing key $url." >&2
  elif GNUPGHOME=$g gpg --batch --quiet --import "$g/key" 2>/dev/null &&
    GNUPGHOME=$g gpg --batch --with-colons --fingerprint "$fpr" 2>/dev/null | grep -qx "fpr:::::::::$fpr:" &&
    GNUPGHOME=$g gpg --batch ${4:+--armor} --export "$fpr" > "$g/out" && [ -s "$g/out" ]; then
    mkdir -p "$(dirname "$out")" && install -m 644 "$g/out" "$out" && ok=1
  else
    echo "The signing key at $url isn't the expected one ($fpr), so it won't be used." >&2
  fi
  GNUPGHOME=$g gpgconf --kill all >/dev/null 2>&1 || true
  rm -rf "$g"
  [ -n "$ok" ]
}
# Tailscale, unless it's installed: from Tailscale's repository with apt
# (Debian, Ubuntu and their derivatives) or dnf/yum (Fedora, RHEL and
# RHEL-compatible systems).
tailscale_install() {
  command -v tailscale >/dev/null && return 0
  local repo=https://pkgs.tailscale.com/stable id path="" code="" pm
  local other="Install Tailscale (https://tailscale.com/download/linux) and run this again."
  id=$(os_field ID)
  echo "Installing Tailscale from its package repository…"
  if command -v apt-get >/dev/null; then
    case "$id" in
      ubuntu | debian) path=$id code=$(os_field VERSION_CODENAME) ;;
      *)
        code=$(os_field UBUNTU_CODENAME) path=ubuntu
        [ -n "$code" ] || { code=$(os_field DEBIAN_CODENAME) path=debian; }
        ;;
    esac
    [ -n "$code" ] || { echo "Pyo can't tell which Debian or Ubuntu release this is. $other" >&2; exit 1; }
    signing_key "$repo/$path/$code.noarmor.gpg" "$TAILSCALE_KEY" /usr/share/keyrings/tailscale-archive-keyring.gpg || exit 1
    echo "deb [signed-by=/usr/share/keyrings/tailscale-archive-keyring.gpg] $repo/$path $code main" > /etc/apt/sources.list.d/tailscale.list
    apt-get update -qq && apt-get install -y -qq tailscale >/dev/null
  elif pm=$(command -v dnf || command -v yum); then
    case "$id" in
      fedora) path=fedora ;;
      rhel | centos | rocky | almalinux | ol) path=rhel/$(os_field VERSION_ID | cut -d. -f1) ;;
      *) echo "Pyo installs Tailscale on Fedora and RHEL-compatible systems, not $id. $other" >&2; exit 1 ;;
    esac
    signing_key "$repo/$path/repo.gpg" "$TAILSCALE_KEY" /etc/pki/rpm-gpg/RPM-GPG-KEY-tailscale armor || exit 1
    cat > /etc/yum.repos.d/tailscale.repo <<REPO
[tailscale-stable]
name=Tailscale stable
baseurl=$repo/$path/\$basearch
enabled=1
type=rpm
repo_gpgcheck=1
gpgcheck=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-tailscale
REPO
    "$pm" install -y -q tailscale
  else
    echo "Pyo installs Tailscale with apt, dnf or yum, and this system has none. $other" >&2
    exit 1
  fi
  command -v tailscale >/dev/null || { echo "Tailscale didn't install (see above)." >&2; exit 1; }
}
# <<< pyo:repos

# >>> pyo:tailnet-lock (the same in connect.sh, join.sh and deploy/sandbox.sh)
# With Tailnet Lock on, a new machine waits until a signing device signs
# its key: the command to run there, then the wait (up to an hour).
tailnet_lock_wait() {
  local js key
  js=$(tailscale lock status --json 2>/dev/null) || return 0
  grep -Eq '"Enabled": *true' <<<"$js" || return 0
  grep -Eq '"NodeKeySigned": *true' <<<"$js" && return 0
  key=$(sed -n 's/^ *"NodeKey": *"\([^"]*\)".*/\1/p' <<<"$js" | head -n1)
  echo "Your tailnet has Tailnet Lock on. On a signing device, run:"
  echo "  tailscale lock sign $key"
  if wait_until "Waiting for the signature…" 3600 tailnet_signed; then
    echo "Signed."
    return 0
  fi
  echo "This machine isn't signed yet. Sign it and run this again." >&2
  exit 1
}
tailnet_signed() { tailscale lock status --json 2>/dev/null | grep -Eq '"NodeKeySigned": *true'; }
# <<< pyo:tailnet-lock
# >>> pyo:k3s-tailnet (the same in connect.sh, join.sh and deploy/install.sh)
# k3s over the tailnet: the node's address and flannel's interface are
# Tailscale's, in their own drop-in. With names, the Kubernetes API's
# certificate names them too. Succeeds when it wrote something new.
K3S_TAILNET_CONFIG=${K3S_TAILNET_CONFIG:-/etc/rancher/k3s/config.yaml.d/40-pyo-tailnet.yaml}
k3s_tailnet_config() { # IP [NAME...]
  local ip=$1 new n
  shift
  new=$(
    printf 'node-ip: "%s"\nflannel-iface: tailscale0\n' "$ip"
    if [ $# -gt 0 ]; then
      echo "tls-san+:"
      for n in "$@"; do printf '  - "%s"\n' "$n"; done
    fi
  )
  [ -f "$K3S_TAILNET_CONFIG" ] && [ "$(cat "$K3S_TAILNET_CONFIG")" = "$new" ] && return 1
  mkdir -p "$(dirname "$K3S_TAILNET_CONFIG")"
  printf '%s\n' "$new" > "$K3S_TAILNET_CONFIG"
}
# <<< pyo:k3s-tailnet
# Pyo's own server, once it's on the tailnet (--tailnet-only): k3s moves
# to it with one restart, then waits for the server and Pyo to be ready.
# If k3s doesn't come back, the drop-in goes back as it was, k3s restarts
# again, and this exits with 3.
k3s_ready() {
  local node i d
  node=$(hostname | tr 'A-Z' 'a-z')
  for i in $(seq 120); do
    [ "$(k3s kubectl get node "$node" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null)" = "$1" ] &&
      k3s kubectl wait --for=condition=Ready "node/$node" --timeout=5s >/dev/null 2>&1 && break
    sleep 5
  done
  [ "$i" -lt 120 ] || return 1
  for d in $(k3s kubectl -n pyo-system get deployments,statefulsets -o name 2>/dev/null); do
    k3s kubectl -n pyo-system rollout status "$d" --timeout=300s >/dev/null 2>&1 || return 1
  done
}
k3s_move_to_tailnet() {
  local old="" node
  [ -f "$K3S_TAILNET_CONFIG" ] && old=$(cat "$K3S_TAILNET_CONFIG")
  k3s_tailnet_config "$TS_IP" "$TS_IP" ${TS_NAME:+"$TS_NAME"} || return 0
  echo "Moving k3s to your tailnet. It restarts once. Services keep running."
  systemctl restart k3s
  if quietly "Waiting for k3s…" k3s_ready "$TS_IP"; then
    # Flannel's address for this server, as other servers reach it.
    node=$(hostname | tr 'A-Z' 'a-z')
    k3s kubectl annotate node "$node" --overwrite "flannel.alpha.coreos.com/public-ip=$TS_IP" >/dev/null 2>&1 || true
    echo "k3s is on your tailnet."
    return 0
  fi
  echo "k3s didn't come back on your tailnet. Putting it back." >&2
  if [ -n "$old" ]; then printf '%s\n' "$old" > "$K3S_TAILNET_CONFIG"; else rm -f "$K3S_TAILNET_CONFIG"; fi
  systemctl restart k3s
  k3s_ready "$(k3s kubectl get node "$(hostname | tr 'A-Z' 'a-z')" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null)" || true
  echo "k3s couldn't move to the tailnet, so it's back as it was."
  exit 3
}
# >>> pyo:tailnet-switch (connect.sh only; join.rs tests it)
# On another tailnet than --tailnet with a key for that one: logged out of
# the old one first, so the key puts it on the new one. Run again once it's
# there, nothing changes.
tailnet_switch() {
  local now
  [ -n "$TAILNET" ] && [ -n "$TS_AUTHKEY" ] || return 0
  [ -n "$(tailscale ip -4 2>/dev/null)" ] || return 0
  now=$(tailscale status --json 2>/dev/null | sed -n 's/^ *"MagicDNSSuffix": *"\([^"]*\)",*$/\1/p' | head -n1 || true)
  now=${now%.}
  [ "$now" != "$TAILNET" ] || return 0
  echo "Leaving ${now:-the old tailnet} for $TAILNET."
  tailscale logout || true
}
# <<< pyo:tailnet-switch
# Tailscale, when Pyo is on your tailnet (or this machine already is).
# TS_NAME: this machine's MagicDNS name, when the tailnet has MagicDNS.
TS_IP="" TS_NAME=""
if [ -n "$TS_AUTHKEY" ] || [ -n "$TAILNET_ONLY" ] || command -v tailscale >/dev/null; then
  command -v tailscale >/dev/null || quietly "Installing Tailscale…" tailscale_install || exit 1
  systemctl enable --now tailscaled
  tailnet_switch
  if [ -z "$(tailscale ip -4 2>/dev/null)" ]; then
    [ -n "$TS_AUTHKEY" ] || { echo "Not on the tailnet yet: copy the full command from Pyo." >&2; exit 1; }
    quietly "Joining your tailnet…" tailscale up --authkey "$TS_AUTHKEY" --hostname "$(hostname)" --timeout 120s || true
  fi
  tailnet_lock_wait
  TS_IP=$(tailscale ip -4 2>/dev/null | head -n1 || true)
  [ -n "$TS_IP" ] || { echo "This machine didn't join your tailnet. Copy a fresh command from Pyo and run it again." >&2; exit 1; }
  TS_NAME=$(tailscale status --json 2>/dev/null | sed -n '/"Self"/,/}/s/^ *"DNSName": *"\([^"]*\)",*$/\1/p' | head -n1 || true)
  TS_NAME=${TS_NAME%.}
fi
if [ -n "$TAILNET_ONLY" ]; then
  echo "$(hostname) is on your tailnet."
  if command -v k3s >/dev/null && systemctl is-active --quiet k3s; then
    k3s_move_to_tailnet
  fi
  exit 0
fi

# Automatic updates: off now, on once the agent runs (at the end).
auto_update_choice
[ "$AUTO_UPDATE" != off ] || auto_update_off
install_url_keep

# >>> pyo:tailnet-host (the same in connect.sh, join.sh and deploy/sandbox.sh)
# A tailnet machine's address, from the MagicDNS name Pyo's commands give
# ("<host>.<tailnet>.ts.net"): its IPv4 from Tailscale's own list of the
# tailnet's machines, so it needn't resolve through DNS here, else from DNS.
# What k3s, the agent and the registry login keep is then the IP, which
# needs no DNS later either. An IP, or a name neither knows, as it is.
tailnet_host() {
  local h=$1 ip=""
  case "$h" in
    *.ts.net | *.ts.net.) ;;
    *) printf '%s\n' "$h"; return 0 ;;
  esac
  if command -v tailscale >/dev/null; then
    ip=$(tailscale ip -4 "${h%.}." 2>/dev/null | head -n1 || true)
  fi
  [ -n "$ip" ] || ip=$(getent ahostsv4 "${h%.}" 2>/dev/null | awk 'NR == 1 { print $1 }' || true)
  printf '%s\n' "${ip:-$h}"
}
# "host:port" with the host as tailnet_host has it.
tailnet_hostport() {
  case "$1" in
    *:*) printf '%s:%s\n' "$(tailnet_host "${1%:*}")" "${1##*:}" ;;
    *) tailnet_host "$1" ;;
  esac
}
# <<< pyo:tailnet-host

# >>> pyo:web-ports (the same in connect.sh and deploy/install.sh)
# What already answers on ports 80 and 443 here, which Pyo Gateway would
# otherwise take over: k3s's Traefik, another load balancer or host port in
# k3s (ingress-nginx and the like), or a program on this machine (nginx,
# Apache, Caddy). Pyo's own gateway doesn't count, nor a program only this
# machine or the tailnet reaches. Sets WEB_PORTS ("80 443", "80" or "443")
# and WEB_BY (what holds them, when that's known). Fails when both are free.
web_ports_busy() {
  local ns name rest addr proc p by80="" by443=""
  WEB_PORTS="" WEB_BY=""
  if command -v k3s >/dev/null && k3s kubectl get --raw=/readyz --request-timeout=10s >/dev/null 2>&1; then
    # Load balancers on 80 or 443: ServiceLB publishes them on every server.
    while read -r ns name rest; do
      [ -n "$name" ] && [ "$ns" != pyo-gateway ] || continue
      for p in $rest; do web_port_taken "$p" "$(web_port_user "$ns" "$name")"; done
    done < <(k3s kubectl get services -A -o jsonpath='{range .items[?(@.spec.type=="LoadBalancer")]}{.metadata.namespace} {.metadata.name}{range .spec.ports[*]} {.port}{end}{"\n"}{end}' 2>/dev/null || true)
    # Pods with host port 80 or 443 (not ServiceLB's own: counted above).
    while read -r ns name rest; do
      [ -n "$name" ] || continue
      case "$ns/$name" in pyo-gateway/* | */svclb-*) continue ;; esac
      for p in $rest; do web_port_taken "$p" "$(web_port_user "$ns" "$name")"; done
    done < <(k3s kubectl get pods -A --field-selector=status.phase!=Succeeded,status.phase!=Failed -o jsonpath='{range .items[*]}{.metadata.namespace} {.metadata.name}{range .spec.containers[*].ports[*]} {.hostPort}{end}{"\n"}{end}' 2>/dev/null || true)
    # k3s's Traefik on its way: its chart is there, its Service not yet.
    if k3s kubectl -n kube-system get helmcharts.helm.cattle.io traefik >/dev/null 2>&1 &&
      ! k3s kubectl -n kube-system get service traefik >/dev/null 2>&1; then
      web_port_taken 80 Traefik
      web_port_taken 443 Traefik
    fi
  fi
  # Programs listening here on an address others reach: not loopback, not
  # the tailnet.
  while read -r addr proc; do
    [ -n "$addr" ] || continue
    p=${addr##*:} addr=${addr%:*}
    case "${addr%%%*}" in
      127.* | '[::1]' | '[::ffff:127.'*) continue ;;
      100.6[4-9].* | 100.[7-9][0-9].* | 100.1[01][0-9].* | 100.12[0-7].* | '[fd7a:115c:a1e0:'*) continue ;;
    esac
    web_port_taken "$p" "$(web_program "$proc")"
  done < <(ss -Hltnp '( sport = :80 or sport = :443 )' 2>/dev/null | awk '{ print $4, $NF }' || true)
  [ -z "$by80" ] || WEB_PORTS=80
  [ -z "$by443" ] || WEB_PORTS="${WEB_PORTS:+$WEB_PORTS }443"
  WEB_BY=${by80:-$by443}
  [ "$WEB_BY" != "?" ] || WEB_BY=""
  [ -n "$WEB_PORTS" ]
}
# web_port_taken PORT NAME: the first user of port 80 or 443 ("?": unknown).
web_port_taken() {
  case "$1" in
    80) [ -n "$by80" ] || by80=${2:-?} ;;
    443) [ -n "$by443" ] || by443=${2:-?} ;;
  esac
}
# web_port_user NAMESPACE NAME: what a Service or pod in k3s is, by name.
web_port_user() {
  case "$1/$2" in
    */*traefik*) echo Traefik ;;
    ingress-nginx/* | */*ingress-nginx*) echo ingress-nginx ;;
    */*nginx*) echo nginx ;;
    */*caddy*) echo Caddy ;;
    */*haproxy*) echo HAProxy ;;
    *) echo "$2" ;;
  esac
}
# web_program 'users:(("nginx",pid=1,fd=6))': the program, as ss -p shows it.
web_program() {
  local n
  n=$(sed -n 's/^users:(("\([^"]*\)".*/\1/p' <<<"$1")
  case "$n" in
    nginx*) echo nginx ;;
    apache2 | httpd) echo Apache ;;
    caddy) echo Caddy ;;
    traefik) echo Traefik ;;
    haproxy) echo HAProxy ;;
    docker-proxy) echo Docker ;;
    *) echo "$n" ;;
  esac
}
# The line that says so, after web_ports_busy.
web_ports_line() {
  local what="Ports 80 and 443 are" by=""
  case "$WEB_PORTS" in 80) what="Port 80 is" ;; 443) what="Port 443 is" ;; esac
  [ -z "$WEB_BY" ] || by=" by $WEB_BY"
  echo "$what in use$by. Public traffic stays off, so your sites keep working."
}
# <<< pyo:web-ports

# Pyo by its tailnet name, now that this machine is on the tailnet: dialled
# at its IP, which is also the address the agent is given (pods needn't
# resolve MagicDNS names). Its registry door the same way.
API=$(tailnet_url "$API")
INSTALL_URL=$(tailnet_url "${INSTALL_URL:-$API}")
[ -z "$REGISTRY" ] || REGISTRY=$(tailnet_hostport "$REGISTRY")

# >>> pyo:foreign-operators (connect.sh only; join.rs tests it)
# cert-manager and Envoy Gateway that Pyo didn't install. Pyo installs its
# own through the agent's manifest (HelmCharts in kube-system) and Envoy
# Gateway's CRDs before it. Every version labels those
# app.kubernetes.io/managed-by=pyo. Another install would be replaced, or
# fight with Pyo's. Prints a line for each one found, and fails when there's
# none.
foreign_operators() {
  local found="" owner ns
  # A chart of Pyo's names in kube-system: "" (none), "pyo" (Pyo's) or "other".
  chart_owner() {
    local got
    got=$(k3s kubectl -n kube-system get helmcharts.helm.cattle.io "$1" -o jsonpath='{.metadata.name}|{.metadata.labels.app\.kubernetes\.io/managed-by}' 2>/dev/null || true)
    case "$got" in "") echo "" ;; "$1|pyo") echo pyo ;; *) echo other ;; esac
  }
  # Namespaces with a Deployment of these labels.
  deployed_in() { k3s kubectl get deployments -A -l "$1" -o jsonpath='{range .items[*]}{.metadata.namespace}{"\n"}{end}' 2>/dev/null || true; }
  # cert-manager: another chart of its name, one running outside Pyo's
  # namespace, or its CRDs without Pyo's chart.
  owner=$(chart_owner cert-manager) found=""
  [ "$owner" != other ] || found=1
  while read -r ns; do
    [ -n "$ns" ] || continue
    [ "$owner" = pyo ] && [ "$ns" = cert-manager ] && continue
    found=1
  done < <(deployed_in app.kubernetes.io/name=cert-manager)
  if [ -z "$owner" ] && k3s kubectl get crd certificates.cert-manager.io >/dev/null 2>&1; then found=1; fi
  [ -z "$found" ] || echo "This server already runs cert-manager that Pyo didn't install. Pyo won't change it. Remove it, or connect a server without it."
  local any=$found
  # Envoy Gateway: another chart of its name, a controller outside Pyo's
  # namespace, or its CRDs without Pyo's label.
  owner=$(chart_owner envoy-gateway) found=""
  [ "$owner" != other ] || found=1
  while read -r ns; do
    [ -n "$ns" ] || continue
    [ "$ns" = pyo-gateway ] && [ "$owner" != other ] && continue
    found=1
  done < <(deployed_in control-plane=envoy-gateway)
  local label
  label=$(k3s kubectl get crd envoyproxies.gateway.envoyproxy.io -o jsonpath='{.metadata.name}|{.metadata.labels.app\.kubernetes\.io/managed-by}' 2>/dev/null || true)
  case "$label" in "" | *"|pyo") ;; *) found=1 ;; esac
  [ -z "$found" ] || echo "This server already runs Envoy Gateway that Pyo didn't install. Pyo won't change it. Remove it, or connect a server without it."
  [ -n "$any$found" ]
}
# Stops the connect, before anything is installed, when foreign_operators
# finds one.
no_foreign_operators() {
  local found
  if found=$(foreign_operators); then
    echo "$found" >&2
    exit 1
  fi
}
# <<< pyo:foreign-operators

# Ports 80 and 443 already in use here (k3s's Traefik, nginx…), on a first
# connect: Pyo connects with public traffic off, so the sites here keep
# working. Pyo turns it off before the agent's first word (install.rs).
WEB_QUERY=""
if ! { command -v k3s >/dev/null && k3s kubectl -n pyo-system get deployment pyo-agent >/dev/null 2>&1; } &&
  web_ports_busy; then
  WEB_QUERY="&webPorts=busy"
fi

# cert-manager or Envoy Gateway that Pyo didn't install, on a k3s that's
# here already: stopped before Pyo installs anything, or uses this token.
if command -v k3s >/dev/null && k3s kubectl get --raw=/readyz --request-timeout=10s >/dev/null 2>&1; then
  no_foreign_operators
fi

# Pyo answers, and knows this token, before anything is installed.
MANIFEST=$(mktemp) CRDS=$(mktemp) K3S_DIR=$(mktemp -d)
trap 'rm -rf "$MANIFEST" "$CRDS" "$K3S_DIR"' EXIT
# >>> pyo:agent-manifest (connect.sh only; join.rs tests it)
code=$(curl -sS -o "$MANIFEST" -w '%{http_code}' --max-time 30 "$INSTALL_URL/install/agent.yaml?token=$TOKEN$WEB_QUERY" || true)
case "$code" in
  200) ;;
  404) echo "Pyo at $INSTALL_URL doesn't know this token. Was the cluster removed, or the token rotated? Copy a fresh command from Pyo." >&2; exit 1 ;;
  409) echo "This command was used already. Get a new one in Pyo: open the cluster and show its install command." >&2; exit 1 ;;
  410) echo "This command expired. Get a new one in Pyo: open the cluster and show its install command." >&2; exit 1 ;;
  000) echo "Can't reach Pyo at $INSTALL_URL from this machine. Is it on the same network (or tailnet), and is port ${INSTALL_URL##*:} open?" >&2; exit 1 ;;
  *)
    echo "Pyo at $INSTALL_URL answered $code for the agent manifest." >&2
    [ -z "$FROM_AGENT" ] || echo "Copy the command from Pyo again and run that." >&2
    exit 1
    ;;
esac
# <<< pyo:agent-manifest

# Firewall: web traffic from anywhere (unless the sites here have 80 and
# 443), cluster traffic over the tailnet.
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  if [ -z "$WEB_QUERY" ]; then
    ufw allow 80/tcp comment 'Pyo ingress' >/dev/null
    ufw allow 443/tcp comment 'Pyo ingress' >/dev/null
  fi
  [ -z "$TS_IP" ] || ufw allow in on tailscale0 comment 'k3s over Tailscale' >/dev/null
fi

# AppArmor: Ubuntu with kernel 6.17+ and
# kernel.apparmor_restrict_unprivileged_unconfined=1 stacks container
# profiles, and containerd's default one then stops processes in a container
# signalling each other (Postgres, Go programs and CoreDNS hang). Install a
# corrected copy before k3s starts, unless someone else's profile is there.
# https://github.com/containerd/containerd/issues/14485
# The profile below is crates/pyo-api/assets/cri-containerd.apparmor.d.
apparmor_workaround() {
  command -v apparmor_parser >/dev/null || return 0
  [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_unconfined 2>/dev/null)" = 1 ] || return 0
  local kv maj min f=/etc/apparmor.d/cri-containerd.apparmor.d
  kv=$(uname -r); maj=${kv%%.*}; min=${kv#*.}; min=${min%%.*}
  [ "$maj" -gt 6 ] || { [ "$maj" -eq 6 ] && [ "$min" -ge 17 ]; } || return 0
  if [ -f "$f" ] && ! head -n1 "$f" | grep -q '^# Managed by Pyo'; then
    return 0
  fi
  cat > "$f" <<'PYO_APPARMOR_PROFILE'
# Managed by Pyo. Remove this file and restart k3s once k3s ships a
# containerd with the fix below.
#
# containerd's default container profile (contrib/apparmor/template.go in
# containerd v2.3.4, as bundled with k3s v1.36.5), with two lines added.
# On Ubuntu with kernel.apparmor_restrict_unprivileged_unconfined=1 and
# kernel 6.17+/7.0, container processes run under the stacked label
# "cri-containerd.apparmor.d//&unconfined", which the stock
# "peer=cri-containerd.apparmor.d" rules don't match: processes in the same
# container can't signal each other (Postgres, Go programs, CoreDNS hang).
# https://github.com/containerd/containerd/issues/14485
# https://github.com/k3s-io/k3s/issues/13625
#
# AppArmor loads this at boot, before k3s; containerd then keeps the
# loaded profile instead of generating its own.

abi <abi/3.0>,

#include <tunables/global>

profile cri-containerd.apparmor.d flags=(attach_disconnected,mediate_deleted) {
  #include <abstractions/base>

  network,
  capability,
  file,
  umount,
  # Host (privileged) processes may send signals to container processes.
  signal (receive) peer=unconfined,
  # runc may send signals to container processes.
  signal (receive) peer=runc,
  # crun may send signals to container processes.
  signal (receive) peer=crun,
  # Manager may send signals to container processes.
  signal (receive) peer=unconfined,
  # Container processes may send signals amongst themselves.
  signal (send,receive) peer=cri-containerd.apparmor.d,
  # Pyo: the same, for the stacked label (the bug above).
  signal (send,receive) peer=cri-containerd.apparmor.d//&unconfined,

  deny @{PROC}/* w,   # deny write for all files directly in /proc (not in a subdir)
  # deny write to files not in /proc/<number>/** or /proc/sys/**
  deny @{PROC}/{[^1-9/],[^1-9/][^0-9/],[^1-9s/][^0-9y/][^0-9s/],[^1-9/][^0-9/][^0-9/][^0-9/]*}/** w,
  deny @{PROC}/sys/[^k]** w,  # deny /proc/sys except /proc/sys/k* (effectively /proc/sys/kernel)
  deny @{PROC}/sys/kernel/{?,??,[^s][^h][^m]**} w,  # deny everything except shm* in /proc/sys/kernel/
  deny @{PROC}/sysrq-trigger rwklx,
  deny @{PROC}/kcore rwklx,

  deny mount,

  deny /sys/[^f]*/** wklx,
  deny /sys/f[^s]*/** wklx,
  deny /sys/fs/[^c]*/** wklx,
  deny /sys/fs/c[^g]*/** wklx,
  deny /sys/fs/cg[^r]*/** wklx,
  deny /sys/firmware/** rwklx,
  deny /sys/devices/virtual/powercap/** rwklx,
  deny /sys/kernel/security/** rwklx,

  # allow processes within the container to trace each other,
  # provided all other LSM and yama setting allow it.
  ptrace (trace,tracedby,read,readby) peer=cri-containerd.apparmor.d,
  # Pyo: the same, for the stacked label.
  ptrace (trace,tracedby,read,readby) peer=cri-containerd.apparmor.d//&unconfined,
}
PYO_APPARMOR_PROFILE
  chmod 0644 "$f"
  apparmor_parser -r "$f"
}
apparmor_workaround

# NVIDIA GPU: containers use it through the NVIDIA Container Toolkit, which
# k3s looks for when it starts (and then has an "nvidia" runtime). The
# driver is never installed here: that's yours to install (and reboot for).
# The toolkit is installed from NVIDIA's repository on apt and dnf/yum
# systems; elsewhere this says what's missing. Sets GPU_LABELS when the GPU
# is ready for containers, and GPU_TOOLKIT_NEW when the toolkit was just
# installed (a k3s that's already running has to restart to see it). A GPU
# that can't be used yet is labelled pyo/gpu=no-driver or no-toolkit, so
# Pyo can say what's missing.
GPU_LABELS="" GPU_TOOLKIT_NEW=""
nvidia_gpu() {
  local d found="" model mem pm
  for d in /sys/bus/pci/devices/*; do
    [ "$(cat "$d/vendor" 2>/dev/null)" = 0x10de ] || continue
    case "$(cat "$d/class" 2>/dev/null)" in 0x0300*|0x0302*) found=1 ;; esac
  done
  [ -n "$found" ] || return 0
  if ! nvidia-smi -L >/dev/null 2>&1; then
    echo "This machine has an NVIDIA GPU, but no working driver (nvidia-smi fails), so Pyo won't use it." >&2
    echo "  Install the driver, reboot, and run this again. On Ubuntu: sudo ubuntu-drivers install" >&2
    echo "  Elsewhere: https://docs.nvidia.com/datacenter/tesla/driver-installation-guide/" >&2
    echo "  Carrying on without the GPU." >&2
    GPU_LABELS="pyo/gpu=no-driver"
    return 0
  fi
  if ! command -v nvidia-container-runtime >/dev/null; then
    echo "Setting up the GPU for containers…"
    if command -v apt-get >/dev/null; then
      if signing_key https://nvidia.github.io/libnvidia-container/gpgkey "$NVIDIA_KEY" /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg; then
        # shellcheck disable=SC2016 # $(ARCH) is apt's
        echo 'deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://nvidia.github.io/libnvidia-container/stable/deb/$(ARCH) /' \
          > /etc/apt/sources.list.d/nvidia-container-toolkit.list
        apt-get update -qq && apt-get install -y -qq nvidia-container-toolkit >/dev/null
      fi
    elif pm=$(command -v dnf || command -v yum); then
      if signing_key https://nvidia.github.io/libnvidia-container/gpgkey "$NVIDIA_KEY" /etc/pki/rpm-gpg/RPM-GPG-KEY-nvidia-container-toolkit armor; then
        cat > /etc/yum.repos.d/nvidia-container-toolkit.repo <<'REPO'
[nvidia-container-toolkit]
name=nvidia-container-toolkit
baseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch
repo_gpgcheck=1
gpgcheck=0
enabled=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-nvidia-container-toolkit
REPO
        "$pm" install -y -q nvidia-container-toolkit
      fi
    else
      echo "Pyo can't install the NVIDIA Container Toolkit on this system. Install it" >&2
      echo "  (https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)" >&2
      echo "  and run this again. Carrying on without the GPU." >&2
      GPU_LABELS="pyo/gpu=no-toolkit"
      return 0
    fi
    command -v nvidia-container-runtime >/dev/null || { echo "The NVIDIA Container Toolkit didn't install. Carrying on without the GPU." >&2; GPU_LABELS="pyo/gpu=no-toolkit"; return 0; }
    GPU_TOOLKIT_NEW=1
  fi
  # The model as a label value: "NVIDIA GeForce RTX 4090" -> NVIDIA-GeForce-RTX-4090.
  model=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -n1 | tr -c 'A-Za-z0-9._-' '-' | cut -c1-63 | sed 's/--*/-/g; s/^[-._]*//; s/[-._]*$//')
  mem=$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits 2>/dev/null | head -n1 | tr -dc 0-9)
  GPU_LABELS="pyo/gpu=nvidia${model:+ pyo/gpu-model=$model}${mem:+ pyo/gpu-memory-mib=$mem}"
  echo "The GPU is ready for containers."
}
nvidia_gpu

# k3s server, unless this machine already runs k3s.
mkdir -p /etc/rancher/k3s
# >>> pyo:registry-login (connect.sh only; join.rs tests it)
# Images Pyo builds, through its registry door with this cluster's login.
# k3s reads the file only when it starts. REGISTRY_CHANGED is set when this
# wrote something else than was there, so a k3s that's running restarts
# for a new address or login and not on every run.
REGISTRY_CHANGED=""
registry_login() {
  local file=/etc/rancher/k3s/registries.yaml want
  want=$(cat <<REG
mirrors:
  "127.0.0.1:5005":
    endpoint:
      - "http://$REGISTRY"
configs:
  "$REGISTRY":
    auth:
      username: pyo-cluster
      password: "$REGISTRY_TOKEN"
REG
  )
  if [ "$want" != "$(cat "$file" 2>/dev/null)" ]; then
    (umask 077 && printf '%s\n' "$want" > "$file")
    REGISTRY_CHANGED=1
  fi
  chmod 600 "$file"
}
# <<< pyo:registry-login
[ -z "$REGISTRY" ] || registry_login
# Its own registry (--local-registry): this machine pulls the images built
# here from 127.0.0.1:5005, which pyo-registry-host forwards to the registry.
LOCAL_REGISTRY_NEW=""
if [ -n "$LOCAL_REGISTRY" ] && ! grep -qs '"127.0.0.1:5005"' /etc/rancher/k3s/registries.yaml; then
  if [ -s /etc/rancher/k3s/registries.yaml ]; then
    echo "/etc/rancher/k3s/registries.yaml has other settings. Add a mirror for 127.0.0.1:5005 (http://127.0.0.1:5005) to it, then run this again." >&2
    exit 1
  fi
  (umask 077 && cat > /etc/rancher/k3s/registries.yaml <<'REG'
mirrors:
  "127.0.0.1:5005":
    endpoint:
      - "http://127.0.0.1:5005"
REG
  )
  LOCAL_REGISTRY_NEW=1
fi
if ! command -v k3s >/dev/null; then
  # Pyo's k3s (deploy/k3s.env), downloaded and checked before it's set up.
  quietly "Downloading k3s…" k3s_fetch "$K3S_DIR" "$PYO_K3S_VERSION" ||
    { echo "Couldn't download k3s. Check this machine's internet connection and run the same command again." >&2; exit 1; }
  # Secrets are encrypted in the datastore with secretbox (XSalsa20-Poly1305).
  # Pyo's gateway (Envoy Gateway, from the agent's manifest) answers
  # on 80/443, not k3s's Traefik.
  cat > /etc/rancher/k3s/config.yaml <<CFG
secrets-encryption: true
secrets-encryption-provider: secretbox
disable:
  - traefik
CFG
  if [ -n "$GPU_LABELS" ]; then
    echo "node-label:" >> /etc/rancher/k3s/config.yaml
    for l in $GPU_LABELS; do echo "  - \"$l\"" >> /etc/rancher/k3s/config.yaml; done
  fi
  # Over the tailnet; the Kubernetes API's certificate also names the
  # machine's MagicDNS name.
  [ -z "$TS_IP" ] || k3s_tailnet_config "$TS_IP" "$TS_IP" ${TS_NAME:+"$TS_NAME"} || true
  small_machine
  quietly "Installing k3s…" k3s_install "$K3S_DIR" server || { echo "k3s didn't install. Run the same command again." >&2; exit 1; }
else
  # Its memory reserve brought up to date; k3s restarts below if it changed.
  k3s_memory_refresh later
  # k3s finds a newly installed NVIDIA toolkit only when it starts, and the
  # registry's login too. It doesn't restart when none of them changed.
  if [ -n "$REGISTRY_CHANGED" ] || [ -n "$LOCAL_REGISTRY_NEW" ] || [ -n "$GPU_TOOLKIT_NEW" ] || [ -n "$K3S_MEMORY_CHANGED" ]; then systemctl restart k3s 2>/dev/null || true; fi
fi

k3s_up() { k3s kubectl get nodes >/dev/null 2>&1; }
wait_until -f "Starting k3s…" 180 k3s_up || { echo "k3s didn't start. Restart this machine, then run the same command again." >&2; exit 1; }
# The same check, for a k3s that wasn't answering before.
no_foreign_operators
# An existing k3s didn't get the labels at registration: add them now.
if [ -n "$GPU_LABELS" ] && k3s kubectl get node "$(hostname)" >/dev/null 2>&1; then
  # shellcheck disable=SC2086 # one label per word
  k3s kubectl label node "$(hostname)" --overwrite $GPU_LABELS >/dev/null || true
fi

# Pyo Gateway's APIs, before the manifest installs Envoy Gateway (it only
# watches kinds whose CRDs exist when it starts): Envoy Gateway's own, and
# the Gateway API's standard channel on a cluster without one. k3s 1.37+
# brings its own Gateway API (and refuses another channel over it): wait for
# that instead. A Gateway API Pyo applied is kept current until k3s brings
# its own; it carries the ownership k3s's chart takes over on upgrade.
gateway_crds() {
  local crd=customresourcedefinition.apiextensions.k8s.io q="" restart=""
  local api="$crd/gateways.gateway.networking.k8s.io"
  if [ -e /var/lib/rancher/k3s/server/manifests/gateway-api-crd.yaml ] || k3s kubectl -n kube-system get helmchart.helm.cattle.io gateway-api-crd >/dev/null 2>&1; then
    if ! k3s kubectl get "$crd/udproutes.gateway.networking.k8s.io" >/dev/null 2>&1; then
      udp_routes() { k3s kubectl get "$crd/udproutes.gateway.networking.k8s.io" >/dev/null 2>&1; }
      wait_until "Getting the gateway ready…" 180 udp_routes || true
      k3s kubectl wait --for=condition=Established --timeout=60s "$api" "$crd/tcproutes.gateway.networking.k8s.io" "$crd/udproutes.gateway.networking.k8s.io" >/dev/null 2>&1 ||
        echo "k3s hasn't installed the Gateway API yet. Domains start answering once it has." >&2
    fi
  elif k3s kubectl get "$api" >/dev/null 2>&1; then
    [ "$(k3s kubectl get "$api" -o jsonpath='{.metadata.labels.pyo\.dev/gateway-api}' 2>/dev/null || true)" != true ] || q="?gatewayApi=1"
  else
    q="?gatewayApi=1"
  fi
  # Envoy Gateway already running without its CRDs: restart it once they're in.
  k3s kubectl get "$crd/backends.gateway.envoyproxy.io" >/dev/null 2>&1 || restart=1
  for i in 1 2 3 4; do
    curl -fsS --max-time 60 -o "$CRDS" "$INSTALL_URL/install/gateway-crds.yaml$q" && break
    [ "$i" = 4 ] && { echo "Couldn't get the gateway's CRDs from Pyo at $INSTALL_URL." >&2; exit 1; }
    sleep 2
  done
  k3s kubectl apply --server-side --force-conflicts --field-manager=pyo -f "$CRDS" >/dev/null || { echo "Couldn't install the gateway's CRDs (see above)." >&2; exit 1; }
  if [ -n "$restart" ] && k3s kubectl -n pyo-gateway get deployment envoy-gateway >/dev/null 2>&1; then
    k3s kubectl -n pyo-gateway rollout restart deployment envoy-gateway >/dev/null || true
  fi
}
gateway_crds

# The cluster's own registry (--local-registry): builders here push to it,
# and every server pulls from it through 127.0.0.1:5005 (loopback only).
local_registry() {
  k3s kubectl apply --server-side --force-conflicts --field-manager=pyo -f - >/dev/null <<'YAML'
apiVersion: v1
kind: Namespace
metadata:
  name: pyo-system
  labels: { pyo.dev/managed: "true" }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: registry-data
  namespace: pyo-system
  labels: { app.kubernetes.io/part-of: pyo }
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 20Gi
---
apiVersion: v1
kind: Service
metadata:
  name: pyo-registry
  namespace: pyo-system
  labels: { app.kubernetes.io/name: registry, app.kubernetes.io/part-of: pyo }
spec:
  selector: { app.kubernetes.io/name: registry }
  ports:
    - { name: registry, port: 5000, targetPort: registry }
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: registry
  namespace: pyo-system
  labels: { app.kubernetes.io/name: registry, app.kubernetes.io/part-of: pyo }
spec:
  replicas: 1
  strategy: { type: Recreate }
  selector:
    matchLabels: { app.kubernetes.io/name: registry }
  template:
    metadata:
      labels: { app.kubernetes.io/name: registry, app.kubernetes.io/part-of: pyo }
    spec:
      containers:
        - name: registry
          image: ghcr.io/upsetdev/pyo/mirror/registry:3.1.2@sha256:82f6ff4a7f982960c8aa51617fea3aabc485df6e55cce80d22deb1de36f1bc3c
          env:
            - { name: REGISTRY_HTTP_ADDR, value: "0.0.0.0:5000" }
            - { name: REGISTRY_STORAGE_DELETE_ENABLED, value: "true" }
            - { name: OTEL_TRACES_EXPORTER, value: "none" }
            - { name: REGISTRY_LOG_LEVEL, value: "warn" }
            - { name: REGISTRY_LOG_ACCESSLOG_DISABLED, value: "true" }
          ports:
            - { name: registry, containerPort: 5000 }
          readinessProbe:
            httpGet: { path: /v2/, port: registry }
            periodSeconds: 30
          volumeMounts:
            - { name: data, mountPath: /var/lib/registry }
          resources:
            requests: { cpu: 10m, memory: 24Mi }
            limits: { memory: 512Mi }
      volumes:
        - name: data
          persistentVolumeClaim: { claimName: registry-data }
---
# Its door (the agent's, registrydoor.rs): the team's registry tokens push
# and pull its own images. The tailnet's registry name routes here.
apiVersion: v1
kind: Service
metadata:
  name: pyo-registry-door
  namespace: pyo-system
  labels: { app.kubernetes.io/name: pyo-registry-door, app.kubernetes.io/part-of: pyo }
spec:
  selector: { app.kubernetes.io/name: pyo-agent }
  ports:
    - { name: door, port: 5006, targetPort: 5006 }
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: pyo-registry-host
  namespace: pyo-system
  labels: { app.kubernetes.io/name: pyo-registry-host, app.kubernetes.io/part-of: pyo }
spec:
  selector:
    matchLabels: { app.kubernetes.io/name: pyo-registry-host }
  template:
    metadata:
      labels: { app.kubernetes.io/name: pyo-registry-host, app.kubernetes.io/part-of: pyo }
    spec:
      hostNetwork: true
      dnsPolicy: ClusterFirstWithHostNet
      tolerations:
        - { operator: Exists }
      containers:
        - name: forward
          image: ghcr.io/upsetdev/pyo/mirror/alpine/socat:1.8.1.3@sha256:5aa280c6fc5bee94dbd2bbdf2a58d30f50396eaa110be84b03636945b157ae68
          args: ["TCP-LISTEN:5005,bind=127.0.0.1,fork,reuseaddr", "TCP:pyo-registry.pyo-system.svc.cluster.local:5000"]
          resources:
            requests: { cpu: 5m, memory: 8Mi }
            limits: { memory: 64Mi }
YAML
}
if [ -n "$LOCAL_REGISTRY" ]; then
  quietly "Setting up this cluster's registry…" local_registry || { echo "Couldn't set up this cluster's registry (see above)." >&2; exit 1; }
fi

# >>> pyo:agent-apply (connect.sh only; join.rs tests it)
# The Kubernetes minor version the API server reports; empty when it doesn't
# answer.
server_minor() {
  { k3s kubectl version -o json 2>/dev/null || true; } |
    sed -n '/"serverVersion"/,/}/s/^ *"minor": *"\([0-9][0-9]*\).*/\1/p'
}
# The Pyo agent and the admission policy that holds it to its limits. Right
# after k3s first starts, or on a small server under load, its API can take
# longer to check the policy than it gives a write by default (34 s): each
# try asks for 2 minutes (the API server caps that at its own limit), as a
# server-side apply (one request per object, without a copy of it in an
# annotation), and tries go on for 5 minutes, with a progress line. Only
# Kubernetes older than 1.30 (it has no admission policies) goes on without
# the policy, quietly.
apply_agent() {
  local minor i delay=5 until=$((SECONDS + 300))
  progress_start "Installing the Pyo agent…"
  while :; do
    if k3s kubectl get --raw=/readyz --request-timeout=20s >/dev/null 2>&1 &&
      k3s kubectl apply --server-side --force-conflicts --field-manager=pyo --request-timeout=120s -f "$MANIFEST" >/dev/null; then
      progress_end
      return 0
    fi
    minor=$(server_minor)
    if [ -n "$minor" ] && [ "$minor" -lt 30 ]; then
      progress_end
      return 0
    fi
    if [ "$SECONDS" -ge "$until" ]; then
      progress_end fail
      return 1
    fi
    for ((i = 0; i < delay; i++)); do
      sleep 1
      progress_tick
    done
    delay=$((delay * 2 > 30 ? 30 : delay * 2))
  done
}
# <<< pyo:agent-apply
# The Pyo agent: it dials out to Pyo; nothing here is exposed.
if ! apply_agent; then
  echo "Couldn't install the Pyo agent (see above). Run the same command again in a minute." >&2
  exit 1
fi
if ! quietly "Starting the Pyo agent…" k3s kubectl -n pyo-system rollout status deployment/pyo-agent --timeout=180s; then
  echo "The Pyo agent didn't start. Run the same command again in a minute." >&2
  exit 1
fi
[ "$AUTO_UPDATE" != on ] || auto_update_on "$INSTALL_URL"
[ -z "$WEB_QUERY" ] || dot_line yellow "$(web_ports_line)"
ok_line "Connected $(hostname). It shows up in Pyo in a few seconds."
