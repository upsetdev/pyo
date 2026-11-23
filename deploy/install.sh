#!/usr/bin/env bash
# Installs Pyo on this machine, or updates it to a newer release:
#
#   curl -fsSL https://pyo.dev/install.sh | bash
#   curl -fsSL https://pyo.dev/install.sh | bash -s -- --version 0.1.0
#
# Options (or environment variables):
#   --version X   a release instead of the latest (PYO_VERSION)
#   --yes         don't ask before an update that has notices (PYO_YES=1)
#   PYO_TELEMETRY=off  don't count this install at api.pyo.dev, and start
#                      Pyo with anonymous usage counts off
#
# Or removes Pyo from this machine (see uninstall_pyo below):
#
#   curl -fsSL https://pyo.dev/install.sh | bash -s -- --uninstall
#
#   --purge       also delete the volumes' data, which is kept otherwise
#   --keep-k3s    keep k3s even when this script installed it
#   --remove-k3s  remove the k3s this script installed even when other
#                 things than Pyo run in it (it's kept then, otherwise)
#   --dry-run     only say what it would do
#
# It installs k3s (unless it's there), then the release the same way Pyo's
# own updates do: the release's upgrader runs as a Job in pyo-system
# (crates/pyo-agent/src/upgrade.rs), applies the release's manifests with its
# images pinned by digest, and waits until everything is ready. Run it again
# to update; it never removes data.
#
# Everything is in functions and only the last line runs anything, so a
# download that stops halfway does nothing.
set -euo pipefail

defaults() {
  NS=pyo-system
  SOURCE=${PYO_UPDATE_SOURCE:-https://api.github.com/repos/upsetdev/pyo}
  VERSION=${PYO_VERSION:-}
  YES=${PYO_YES:-}
  TELEMETRY=${PYO_TELEMETRY:-}
  WORK=""
  UNINSTALL="" PURGE="" KEEP_K3S="" REMOVE_K3S="" DRY_RUN=""
}

fail() {
  progress_end fail
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

kc() { k3s kubectl "$@"; }

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

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --version) VERSION="${2:-}"; shift 2 ;;
      --version=*) VERSION="${1#*=}"; shift ;;
      --yes | -y) YES=1; shift ;;
      --uninstall) UNINSTALL=1; shift ;;
      --purge) PURGE=1; shift ;;
      --keep-k3s) KEEP_K3S=1; shift ;;
      --remove-k3s) REMOVE_K3S=1; shift ;;
      --dry-run) DRY_RUN=1; shift ;;
      -h | --help)
        sed -n '2,20p' "${BASH_SOURCE[0]:-/dev/null}" 2>/dev/null || true
        echo "Usage: install.sh [--version X] [--yes]"
        echo "       install.sh --uninstall [--purge] [--keep-k3s | --remove-k3s] [--dry-run] [--yes] [--version X]"
        exit 0
        ;;
      *) fail "Unknown option: $1 (use --version X or --yes, or --uninstall)" ;;
    esac
  done
  VERSION=${VERSION#v}
  if [ -n "$VERSION" ] && ! valid_version "$VERSION"; then
    fail "\"$VERSION\" isn't a release version, like 0.1.0."
  fi
  if [ -z "$UNINSTALL" ] && [ -n "$PURGE$KEEP_K3S$REMOVE_K3S$DRY_RUN" ]; then
    fail "--purge, --keep-k3s, --remove-k3s and --dry-run go with --uninstall."
  fi
  if [ -n "$KEEP_K3S" ] && [ -n "$REMOVE_K3S" ]; then
    fail "--keep-k3s or --remove-k3s, not both."
  fi
}

valid_version() { [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; }

# a >= b, for versions like 0.1.0.
version_ge() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n1)" = "$2" ]; }

# Root, or again with sudo. Piped from curl there's no file to run again, so
# the script travels as its functions plus the settings it sets outside them
# (SETTINGS, which CI checks against the script). sudo clears the
# environment, so every PYO_* variable goes along too.
SETTINGS="K3S_UNIT K3S_MEMORY_CONFIG K3S_MEMORY_CHANGED K3S_TAILNET_CONFIG COSIGN_VERSION COSIGN_SHA256_AMD64 RELEASE_SIGNER RELEASE_ISSUER"
become_root() {
  [ "$(id -u)" = 0 ] && return 0
  command -v sudo >/dev/null || fail "Run this as root (there's no sudo)."
  say "Pyo needs root. Asking sudo"
  local keep=() v settings
  for v in $(compgen -e -X '!PYO_*'); do keep+=("$v=${!v}"); done
  # shellcheck disable=SC2086 # a list of names
  settings=$(declare -p SETTINGS $SETTINGS)
  # shellcheck disable=SC2016 # expanded by the new shell
  exec sudo env "${keep[@]}" PYO_VERSION="$VERSION" PYO_YES="$YES" PYO_UPDATE_SOURCE="$SOURCE" PYO_TELEMETRY="$TELEMETRY" \
    bash -c "$settings; $(declare -f); main \"\$@\"" install.sh "$@"
}

check_system() {
  [ "$(uname -s)" = Linux ] || fail "Pyo runs on Linux, and this is $(uname -s)."
  case "$(uname -m)" in
    x86_64 | amd64) ;;
    *) fail "Pyo's images are for x86-64 (amd64), and this machine is $(uname -m)." ;;
  esac
  local tool
  for tool in curl sed grep awk sort ss; do
    command -v "$tool" >/dev/null || fail "$tool is missing. Install it and run this again."
  done
}

installed() { command -v k3s >/dev/null && kc -n "$NS" get deployment pyo >/dev/null 2>&1; }

# Ports Pyo takes. 80 and 443 in use (k3s's Traefik, nginx…) don't stop
# it: Pyo goes on with public traffic off, so the sites here keep working
# (connect.sh tells Pyo so), and WEB_PORTS says they're taken.
check_ports() {
  local ports=(8000 5005 5006)
  command -v k3s >/dev/null || ports+=(6443)
  local p busy=""
  for p in "${ports[@]}"; do
    if ss -ltnH "( sport = :$p )" 2>/dev/null | grep -q .; then busy="$busy $p"; fi
  done
  if [ -n "$busy" ]; then
    echo "These ports are in use:$busy" >&2
    ss -ltnpH 2>/dev/null | grep -E ":($(echo "$busy" | xargs | tr ' ' '|'))\b" >&2 || true
    fail "Pyo needs them (8000 for the dashboard, 5005 and 5006 for its registry, 6443 for k3s). Stop what holds them and run this again."
  fi
  if web_ports_busy; then
    dot_line yellow "$(web_ports_line)"
  fi
}

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

# AppArmor: Ubuntu with kernel 6.17+ and
# kernel.apparmor_restrict_unprivileged_unconfined=1 stacks container
# profiles, and containerd's default one then stops processes in a container
# signalling each other (Postgres never starts). Install a corrected copy
# before k3s starts, unless someone else's profile is there. The same as in
# connect.sh; https://github.com/containerd/containerd/issues/14485
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

# A field of release.json's "k3s".
k3s_pin() { sed -n "/^  \"k3s\": {/,/^  }/s/^    \"$1\": \"\([^\"]*\)\",*\$/\1/p" "$WORK/release.json"; }

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

# k3s with Pyo's settings: Secrets encrypted with secretbox, and containerd
# pulling Pyo's own registry (127.0.0.1:5005) over HTTP. An existing k3s is
# used as it is, apart from its memory reserve (k3s_memory_refresh).
install_k3s() {
  if command -v k3s >/dev/null; then
    say "Using the k3s that's installed"
    k3s_memory_refresh
  else
    # The release's k3s (release.json's "k3s", from deploy/k3s.env),
    # downloaded and checked before anything changes.
    PYO_K3S_VERSION=$(k3s_pin version)
    PYO_K3S_INSTALLER_SHA256=$(k3s_pin installerSha256)
    PYO_K3S_SHA256_AMD64=$(k3s_pin amd64Sha256)
    PYO_K3S_SHA256_ARM64=$(k3s_pin arm64Sha256)
    quietly "Downloading k3s…" k3s_fetch "$WORK/k3s" "$PYO_K3S_VERSION" || fail "Couldn't download k3s. Check this machine's internet connection and run the same command again."
    apparmor_workaround
    mkdir -p /etc/rancher/k3s
    cat > /etc/rancher/k3s/config.yaml <<'CONFIG'
# Secrets are encrypted in the datastore with secretbox (XSalsa20-Poly1305).
secrets-encryption: true
secrets-encryption-provider: secretbox
CONFIG
    # Already on a tailnet: k3s runs over it from the start, as connect.sh
    # and join.sh set it up, and the Kubernetes API's certificate names the
    # machine's tailnet address and MagicDNS name. Otherwise Pyo moves it
    # there once Tailscale is set up in Pyo (pyo-tailnet.timer).
    local ts_ip ts_name
    ts_ip=$(tailscale ip -4 2>/dev/null | head -n1 || true)
    ts_name=$(tailscale status --json 2>/dev/null | sed -n '/"Self"/,/}/s/^ *"DNSName": *"\([^"]*\)",*$/\1/p' | head -n1 || true)
    ts_name=${ts_name%.}
    [ -z "$ts_ip" ] || k3s_tailnet_config "$ts_ip" "$ts_ip" ${ts_name:+"$ts_name"} || true
    cat > /etc/rancher/k3s/registries.yaml <<'REGISTRIES'
mirrors:
  "127.0.0.1:5005":
    endpoint:
      - "http://127.0.0.1:5005"
REGISTRIES
    small_machine
    # Pyo Gateway takes 80/443, not k3s's Traefik.
    quietly "Installing k3s…" k3s_install "$WORK/k3s" server --disable traefik || fail "k3s didn't install. Run the same command again."
    # So --uninstall knows k3s is Pyo's to remove.
    install -d -m 700 /etc/pyo
    echo "k3s was installed by Pyo's install.sh on $(date -u +%F)." > /etc/pyo/k3s-installed
  fi
  wait_until -f "Starting k3s…" 180 k3s_up || fail "k3s didn't start. Restart this machine, then run the same command again."
}
k3s_up() { kc get nodes >/dev/null 2>&1; }

# ── The release ─────────────────────────────────────────────────────────────

# Only GitHub and api.pyo.dev, or the configured mirror, and HTTPS unless the
# mirror isn't.
fetch() {
  local proto='=https'
  case "$SOURCE" in http://*) proto='=http,https' ;; esac
  curl -fsSL --proto "$proto" --max-time 60 -H 'Accept: application/vnd.github+json' "$@"
}

# A download, four tries 2 seconds apart, as one can fail on a bad
# connection (k3s_get's loop, not curl's --retry-all-errors, which older
# curl lacks).
tries() {
  local i
  for i in 1 2 3; do
    "$@" && return 0
    sleep 2
  done
  "$@"
}

# The release this script installs: the latest, or --version's (GitHub's
# description of it, with its assets' download addresses). The latest comes
# from api.pyo.dev first, which keeps a copy of GitHub's answer and still
# has it when GitHub fails or limits this address, and from GitHub next. A
# mirror (PYO_UPDATE_SOURCE) is the only place asked. Each gets a few tries,
# and why each failed goes to stderr. It sends no install ID and no
# version, so a lookup counts nothing.
release_meta() {
  local urls=() url i out why
  if [ -n "$VERSION" ]; then
    urls=("$SOURCE/releases/tags/v$VERSION")
  else
    [ "$SOURCE" != https://api.github.com/repos/upsetdev/pyo ] || urls=(https://api.pyo.dev/v1/releases/latest)
    urls+=("$SOURCE/releases/latest")
  fi
  for url in "${urls[@]}"; do
    for i in 1 2 3; do
      if out=$(fetch --connect-timeout 10 --max-time 30 "$url" 2>&1); then
        if [[ $out == *'"tag_name"'* ]]; then
          printf '%s\n' "$out"
          return 0
        fi
        why="not a release's description"
      else
        why=${out##*$'\n'}
        why=${why:-no answer}
      fi
      # A release that isn't there won't be on another try.
      [[ $why != *"error: 404"* ]] || break
      [ "$i" = 3 ] || sleep "$((i * 2))"
    done
    echo "  $url: $why" >&2
  done
  return 1
}

# One of the release's assets' download address.
asset_url() {
  grep -o "\"browser_download_url\": *\"[^\"]*/$1\"" | head -n1 | sed 's/.*"\([^"]*\)"$/\1/'
}

# connect.sh of the same release, into $1 (uninstalling from a k3s that
# stays).
fetch_connect() {
  local meta asset
  meta=$(release_meta 2>/dev/null) || return 1
  asset=$(printf '%s' "$meta" | asset_url 'connect\.sh')
  [ -n "$asset" ] && tries fetch -o "$1" "$asset"
}

# release.json is signed by Pyo's release workflow (cosign sign-blob,
# keyless: the GitHub Actions identity of release.yml at a v* tag, recorded
# in Sigstore's transparency log). Nothing of it is used before that
# signature checks out, with cosign: the one on this machine, or else this
# pinned release of it, itself checked against its SHA-256 here.
COSIGN_VERSION=v3.1.4
COSIGN_SHA256_AMD64=424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa
RELEASE_SIGNER='^https://github\.com/upsetdev/pyo/\.github/workflows/release\.yml@refs/tags/v'
RELEASE_ISSUER=https://token.actions.githubusercontent.com

# cosign, into $COSIGN.
find_cosign() {
  COSIGN=$(command -v cosign || true)
  [ -z "$COSIGN" ] || return 0
  quietly "Downloading the tool that checks it…" tries curl -fsSL --proto '=https' --max-time 300 -o "$WORK/cosign" \
    "https://github.com/sigstore/cosign/releases/download/$COSIGN_VERSION/cosign-linux-amd64" ||
    fail "Couldn't download cosign, which checks the release's signature."
  [ "$(sha256sum "$WORK/cosign" | cut -d' ' -f1)" = "$COSIGN_SHA256_AMD64" ] ||
    fail "The downloaded cosign doesn't match the expected checksum, so it won't run."
  chmod 755 "$WORK/cosign"
  COSIGN=$WORK/cosign
}

# $1: what the release is called, $2: GitHub's description of it.
verify_release() {
  local bundle
  bundle=$(printf '%s' "$2" | asset_url 'release\.json\.sigstore\.json')
  [ -n "$bundle" ] || fail "$1 has no signature for its release.json (release.json.sigstore.json), so it can't be trusted."
  tries fetch -o "$WORK/release.json.sigstore.json" "$bundle" || fail "Couldn't download $bundle."
  say "Checking the release's signature"
  find_cosign
  "$COSIGN" verify-blob --bundle "$WORK/release.json.sigstore.json" \
    --certificate-identity-regexp "$RELEASE_SIGNER" --certificate-oidc-issuer "$RELEASE_ISSUER" \
    "$WORK/release.json" >"$WORK/cosign.log" 2>&1 || {
    cat "$WORK/cosign.log" >&2
    fail "release.json's signature doesn't check out: it isn't the one Pyo's release workflow signed. Nothing was changed."
  }
}

# The release.json asset: release-manifest.ts writes it pretty-printed, two
# spaces, which is what the reads below rely on.
fetch_release() {
  local what meta asset
  if [ -n "$VERSION" ]; then
    what="Pyo $VERSION"
  else
    what="the latest Pyo release"
  fi
  say "Looking up $what"
  meta=$(release_meta 2>"$WORK/lookup.log") || {
    if [ -n "$VERSION" ] && grep -q "error: 404" "$WORK/lookup.log"; then
      fail "There's no release $VERSION. See https://github.com/upsetdev/pyo/releases."
    fi
    cat "$WORK/lookup.log" >&2
    fail "Couldn't look up $what. Tried the addresses above."
  }
  asset=$(printf '%s' "$meta" | asset_url 'release\.json')
  [ -n "$asset" ] || fail "$what has no release.json, so it can't be installed with this script."
  tries fetch -o "$WORK/release.json" "$asset" || fail "Couldn't download $asset."
  [ "$(wc -c < "$WORK/release.json")" -lt 4194304 ] || fail "release.json is too large."
  verify_release "$what" "$meta"

  R_VERSION=$(sed -n 's/^  "version": "\([^"]*\)",*$/\1/p' "$WORK/release.json")
  R_AGENT=$(sed -n '/^  "images": {/,/^  }/s/^    "agent": "\([^"]*\)",*$/\1/p' "$WORK/release.json")
  R_MIN_FROM=$(sed -n 's/^  "minFrom": "\([^"]*\)",*$/\1/p' "$WORK/release.json")
  R_MANUAL=$(sed -n 's/^  "manualOnly": \(true\|false\),*$/\1/p' "$WORK/release.json")
  mapfile -t R_NOTICES < <(sed -n '/^  "beforeYouUpdate": \[$/,/^  \]/s/^    "\(.*\)",*$/\1/p' "$WORK/release.json" | sed 's/\\"/"/g')
  valid_version "$R_VERSION" || fail "release.json isn't one this script can read (no version)."
  [ -z "$VERSION" ] || [ "$R_VERSION" = "$VERSION" ] || fail "The release.json of $VERSION says it's $R_VERSION."
  [[ $R_AGENT =~ ^[a-z0-9.:/_-]+@sha256:[0-9a-f]{64}$ ]] || fail "release.json's agent image isn't pinned by digest."
}

# The version that's running, from the pyo Deployment's image tag ("" if it
# isn't a release, e.g. built from a repo copy).
current_version() {
  local image tag
  image=$(kc -n "$NS" get deployment pyo -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
  tag=${image%@*}
  tag=${tag##*:}
  valid_version "$tag" && echo "$tag" || true
}

# On an update: minFrom, the notices, and an answer when there are any.
check_update() {
  local cur=$1
  if [ -n "$cur" ] && [ "$cur" = "$R_VERSION" ]; then
    return 1
  fi
  if [ -n "$cur" ] && ! version_ge "$R_VERSION" "$cur"; then
    fail "This machine runs Pyo $cur, newer than $R_VERSION. Going back isn't supported."
  fi
  if [ -n "$R_MIN_FROM" ]; then
    if [ -z "$cur" ]; then
      echo "Pyo $R_VERSION updates only from $R_MIN_FROM or later, and the running version is unknown." >&2
      [ -n "$YES" ] || fail "Update to $R_MIN_FROM first (--version $R_MIN_FROM), or run again with --yes if it's at least that."
    elif ! version_ge "$cur" "$R_MIN_FROM"; then
      fail "Pyo $cur can't update straight to $R_VERSION. Update to $R_MIN_FROM first: install.sh --version $R_MIN_FROM"
    fi
  fi
  if [ "${#R_NOTICES[@]}" -gt 0 ] || [ "$R_MANUAL" = true ]; then
    echo
    echo "Before you update to Pyo $R_VERSION:"
    local n
    for n in "${R_NOTICES[@]}"; do echo "  - $n"; done
    [ "${#R_NOTICES[@]}" -gt 0 ] || echo "  - This release is meant to be updated by hand. Read its notes first."
    echo
    if [ -z "$YES" ]; then
      local answer=""
      if [ -r /dev/tty ] && { exec 3</dev/tty; } 2>/dev/null; then
        read -r -p "Update now? [y/N] " answer <&3 || true
        exec 3<&-
      else
        fail "Read the above, then run again with --yes."
      fi
      case "$answer" in y | Y | yes) ;; *) fail "Not updated." ;; esac
    fi
  fi
  return 0
}

# ── Applying it ─────────────────────────────────────────────────────────────

# ── Where Pyo is, and this machine's helpers ───────────────────────────────
# One server needs no Tailscale: Pyo is on this machine's public IP over
# HTTPS (https://pyo.<public IP>.nip.io, Let's Encrypt) until an admin
# closes it, and over plain HTTP on :8000 from this machine, its private
# network and its tailnet. Once Tailscale is set up in Pyo (Integrations),
# this machine joins the tailnet by itself and Pyo is also
# https://pyo.<tailnet>.ts.net. Which addresses there are, and how they're
# shown, is in addresses.sh (host_units writes it).

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
# this machine as a cluster until it's done, putting it on the tailnet
# once Tailscale is set up in Pyo, and refreshing its cluster after a
# release that needs the connect command. Idempotent.
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
#
# After that it keeps this server's cluster current. A release can bring
# what the agent can't write itself (a new role, a chart, host settings):
# Pyo then says "refresh", and connect.sh from Pyo here runs again.
#   auto     from pyo-tailnet's timer, every minute: only that refresh,
#            once per Pyo version and install (refresh-tried lists them).
#            One that failed (refresh-failed) is tried again after an
#            hour, and never more than 4 run in a day (refresh-runs).
#   refresh  from install.sh after an update: that refresh now.
set -euo pipefail
mode=${1:-}
tried=/etc/pyo/refresh-tried
failed=/etc/pyo/refresh-failed
runs=/etc/pyo/refresh-runs
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
# tell INSTALL STATUS: how the refresh goes, for the cluster's settings.
tell() {
  curl -fsS -m 20 -o /dev/null --data-urlencode "key=$key" --data-urlencode "install=$1" \
    --data-urlencode "status=$2" http://127.0.0.1:8000/install/this-machine/refresh 2>/dev/null || true
}
# Automatic runs in the last day.
runs_today() {
  local since=$(($(date +%s) - 86400))
  awk -v since="$since" '$1 + 0 > since' "$runs" 2>/dev/null | wc -l
}
# refresh INSTALL: connect.sh from Pyo here, for that install.
refresh() {
  local want=$1 rc=0
  if [ "$mode" = auto ]; then
    if grep -qxF -- "$want" "$tried" 2>/dev/null; then
      # It ran for this install. Again only an hour after it failed.
      if [ "$(cat "$failed" 2>/dev/null)" != "$want" ]; then
        tell "$want" done
        return 0
      fi
      if [ -z "$(find "$failed" -mmin +60 2>/dev/null)" ]; then
        tell "$want" failed
        return 0
      fi
    fi
    if [ "$(runs_today)" -ge 4 ]; then
      tell "$want" held
      return 0
    fi
    { awk -v since="$(($(date +%s) - 86400))" '$1 + 0 > since' "$runs" 2>/dev/null || true; date +%s; } > "$runs.new"
    mv -f "$runs.new" "$runs"
  fi
  { grep -vxF -- "$want" "$tried" 2>/dev/null | tail -n 20 || true; echo "$want"; } > "$tried.new"
  mv -f "$tried.new" "$tried"
  # Failed until it's through: one cut short (a restart) is tried again too.
  echo "$want" > "$failed"
  tell "$want" running
  echo "Refreshing this server for Pyo ${want%-*}."
  curl -fsS -m 60 http://127.0.0.1:8000/install/connect.sh | bash || rc=$?
  if [ "$rc" = 0 ]; then
    rm -f "$failed"
    tell "$want" done
    echo "This server is up to date."
  else
    tell "$want" failed
    echo "Couldn't refresh this server. Pyo tries again in an hour."
  fi
  return "$rc"
}
# One run at a time (the timers, the setup's own run and connect.sh run by
# hand): a second one waits, then finds this machine connected, or runs the
# same command again. The automatic refresh doesn't wait: it comes back in
# a minute.
exec 9>/run/pyo-connect.lock
if [ "$mode" = auto ]; then flock -n 9 || exit 0; else flock 9; fi
key=$(cat /etc/pyo/bootstrap-key)
r=$(curl -fsS -m 20 "http://127.0.0.1:8000/install/this-machine?key=$key") || exit 0
# The install this Pyo brings, when it keeps this server current.
install=$(sed -n 's/.*"install":"\([0-9A-Za-z._+-]*\)".*/\1/p' <<<"$r")
case "$r" in
  *'"state":"connected"'*)
    if [ "$mode" = auto ]; then
      case "$r" in *'"refresh":true'*) [ -z "$install" ] || refresh "$install" || true ;; esac
      exit 0
    fi
    if [ "$mode" = refresh ] && [ -n "$install" ]; then
      refresh "$install"
    elif ! policy_ok; then
      # Without its policy (k3s didn't answer in time): connect.sh again,
      # which reads the agent's token from the cluster.
      curl -fsS -m 60 http://127.0.0.1:8000/install/connect.sh | bash
    fi
    if policy_ok; then done_; fi
    ;;
  *'"state":"connect"'*)
    [ "$mode" != auto ] || exit 0
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
if [ "$sum" != "$(cat /etc/pyo/host-units 2>/dev/null)" ]; then
  exec bash -c "$units"$'\n'"echo $sum > /etc/pyo/host-units"
fi
# With this Pyo's scripts in place: this server's refresh, when Pyo asks
# for one (connect-this-machine.sh).
exec /usr/local/lib/pyo/connect-this-machine.sh auto
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
  local m=169.254.169.254 t="" h=() rc=0
  case $1 in
    aws) # AWS (IMDSv2, else v1), and clouds that answer like it. No answer
      # at all (not a cloud): no second request.
      t=$(curl -fsS -m 1 -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' "http://$m/latest/api/token" 2>/dev/null) || rc=$?
      [ "$rc" = 0 ] || [ "$rc" = 22 ] || return 0
      [ -z "$t" ] || h=(-H "X-aws-ec2-metadata-token: $t")
      curl -fsS -m 1 "${h[@]}" "http://$m/latest/meta-data/public-ipv4" ;;
    gcp) curl -fsS -m 1 -H 'Metadata-Flavor: Google' "http://$m/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip" ;;
    azure) curl -fsS -m 1 -H 'Metadata: true' "http://$m/metadata/instance/network/interface/0/ipv4/ipAddress/0/publicIpAddress?api-version=2021-02-01&format=text" ;;
    alibaba)
      t=$(curl -fsS -m 1 -X PUT -H 'X-aliyun-ecs-metadata-token-ttl-seconds: 60' http://100.100.100.200/latest/api/token 2>/dev/null) || rc=$?
      [ "$rc" = 0 ] || [ "$rc" = 22 ] || return 0
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

random_hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }

# A new install's first sign-in (Pyo creates the admin with it at first start).
first_sign_in() {
  ADMIN_PASSWORD=""
  if ! kc -n "$NS" get secret pyo-admin >/dev/null 2>&1; then
    ADMIN_PASSWORD=$(random_hex 16)
    kc -n "$NS" create secret generic pyo-admin --from-literal=password="$ADMIN_PASSWORD" >/dev/null
  else
    # An install that stopped halfway: the same login as before.
    ADMIN_PASSWORD=$(kc -n "$NS" get secret pyo-admin -o jsonpath='{.data.password}' | base64 -d 2>/dev/null || true)
  fi
}

telemetry_off() {
  case "${TELEMETRY,,}" in
    off | false | 0 | no) return 0 ;;
  esac
  return 1
}

# A fresh install told PYO_TELEMETRY=off: the new Pyo starts with anonymous
# usage counts off too. Pyo reads the pyo-install ConfigMap once, with its
# first admin, and Settings > Updates changes it later.
install_settings() {
  if telemetry_off; then
    kc -n "$NS" create configmap pyo-install --from-literal=telemetry=off --dry-run=client -o yaml | kc apply -f - >/dev/null
  else
    kc -n "$NS" delete configmap pyo-install --ignore-not-found >/dev/null
  fi
}

# The release's upgrader, as the agent runs it for dashboard updates (the
# objects match upgrade.rs's upgrader_rbac and run_upgrader). $1: a fresh
# install.
run_upgrader() {
  local fresh=${1:-} id name
  id="install-$(date +%s)"
  name="pyo-upgrade-$id"
  # The namespace as platform.yaml has it (the upgrader skips Namespaces).
  kc apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Namespace
metadata: { name: $NS, labels: { app.kubernetes.io/part-of: pyo } }
YAML
  first_sign_in
  [ -z "$fresh" ] || install_settings
  bootstrap_key kc
  kc -n "$NS" create configmap "$name" --from-file=release.json="$WORK/release.json" >/dev/null
  # >>> pyo:upgrader-rbac (upgrade.rs's upgrader_rbac, as JSON; cargo test compares them)
  kc apply --server-side --force-conflicts --field-manager=pyo -f - >/dev/null <<'JSON'
{"apiVersion": "v1", "kind": "List", "items": [
  {"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": "pyo-upgrade", "namespace": "pyo-system", "labels": {"app.kubernetes.io/managed-by": "pyo"}}},
  {"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role", "metadata": {"name": "pyo-upgrade", "namespace": "pyo-system", "labels": {"app.kubernetes.io/managed-by": "pyo"}}, "rules": [
    {"apiGroups": ["apps"], "resources": ["deployments", "statefulsets"], "verbs": ["get", "list", "watch", "create", "patch"]},
    {"apiGroups": [""], "resources": ["services", "configmaps", "persistentvolumeclaims", "serviceaccounts"], "verbs": ["get", "create", "patch"]},
    {"apiGroups": [""], "resources": ["secrets"], "verbs": ["get", "create"]},
    {"apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list", "delete"]},
    {"apiGroups": ["policy"], "resources": ["poddisruptionbudgets"], "verbs": ["get", "create", "patch"]},
    {"apiGroups": ["networking.k8s.io"], "resources": ["networkpolicies"], "verbs": ["get", "create", "patch"]}
  ]},
  {"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": {"name": "pyo-upgrade", "namespace": "pyo-system", "labels": {"app.kubernetes.io/managed-by": "pyo"}},
   "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": "pyo-upgrade"},
   "subjects": [{"kind": "ServiceAccount", "name": "pyo-upgrade", "namespace": "pyo-system"}]}
]}
JSON
  # <<< pyo:upgrader-rbac
  kc apply --server-side --force-conflicts --field-manager=pyo -f - >/dev/null <<YAML
apiVersion: batch/v1
kind: Job
metadata: { name: $name, namespace: $NS, labels: { app.kubernetes.io/managed-by: pyo, app.kubernetes.io/name: pyo-upgrade, pyo/update-id: $id } }
spec:
  backoffLimit: 0
  activeDeadlineSeconds: 1800
  ttlSecondsAfterFinished: 86400
  template:
    metadata: { labels: { app.kubernetes.io/managed-by: pyo, app.kubernetes.io/name: pyo-upgrade, pyo/update-id: $id } }
    spec:
      serviceAccountName: pyo-upgrade
      restartPolicy: Never
      containers:
        - name: upgrade
          image: "$R_AGENT"
          args: [upgrade, /release/release.json]
          env:
            - { name: RUST_LOG, value: pyo_agent=info }
          volumeMounts: [{ name: release, mountPath: /release, readOnly: true }]
          resources: { requests: { cpu: 20m, memory: 32Mi }, limits: { memory: 256Mi } }
          securityContext: { runAsNonRoot: true, runAsUser: 65532, allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: [ALL] } }
      volumes: [{ name: release, configMap: { name: $name } }]
YAML
  progress_start "Starting Pyo $R_VERSION${fresh:+ (a few minutes the first time)}…"
  local i st pod why
  for i in $(seq 380); do
    st=$(kc -n "$NS" get job "$name" -o jsonpath='{.status.succeeded}/{.status.failed}' 2>/dev/null || true)
    case "$st" in 1/* | */1) break ;; esac
    pod=$(kc -n "$NS" get pods -l "pyo/update-id=$id" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)
    if [ -n "$pod" ]; then
      why=$(kc -n "$NS" get pod "$pod" -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null || true)
      case "$why" in
        ErrImagePull | ImagePullBackOff | InvalidImageName)
          if [ "$i" -gt 60 ]; then
            kc -n "$NS" delete job "$name" --wait=false >/dev/null 2>&1 || true
            fail "This machine can't download Pyo. Check its internet connection and run the same command again. Nothing was changed."
          fi
          ;;
      esac
    fi
    for _ in 1 2 3 4 5; do
      sleep 1
      progress_tick
    done
  done
  local logs outcome
  logs=$(kc -n "$NS" logs "job/$name" 2>/dev/null || true)
  outcome=$(printf '%s\n' "$logs" | grep pyoUpgradeOutcome | tail -n1 || true)
  kc -n "$NS" delete configmap "$name" >/dev/null 2>&1 || true
  # Its last line says how it went; without it (logs gone), the Job's state.
  # When it failed, what it did goes to stderr first.
  case "$outcome" in
    *'"ok":true'*) progress_end ok "Started Pyo $R_VERSION" ;;
    *'"ok":false'*)
      progress_end fail
      upgrader_log "$logs"
      local why
      why=$(printf '%s' "$outcome" | sed -n 's/.*"message":"\([^"\\]*\)".*/\1/p' | head -n1)
      [ -n "$why" ] || why="Pyo $R_VERSION didn't start. The version that was running keeps running. Run the same command again to try again."
      fail "$why"
      ;;
    *)
      case "$st" in
        1/*) progress_end ok "Started Pyo $R_VERSION" ;;
        */1)
          progress_end fail
          upgrader_log "$logs"
          fail "Pyo $R_VERSION didn't start. The version that was running keeps running. Run the same command again to try again."
          ;;
        *)
          progress_end fail
          upgrader_log "$logs"
          fail "Pyo $R_VERSION didn't start in time. Run the same command again to try again."
          ;;
      esac
      ;;
  esac
}

# What the upgrader did, its last lines, when it failed.
upgrader_log() {
  [ -n "$1" ] || return 0
  printf '%s\n' "$1" | grep -v pyoUpgradeOutcome | tail -n 20 | sed 's/^/    /' >&2
}

# Counts a fresh install at api.pyo.dev, once the dashboard answers: Pyo's
# version, the OS and the arch, nothing else (no hostname, address or
# owner). PYO_TELEMETRY=off skips it, and turns usage counts off
# (install_settings). A failure changes nothing. Silent: the docs say what
# it sends and how to turn it off.
count_install() {
  telemetry_off && return 0
  local arch
  case "$(uname -m)" in
    x86_64 | amd64) arch=amd64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *) return 0 ;;
  esac
  dashboard_up || return 0
  curl -fsS --proto '=https' -m 5 -o /dev/null -H 'Content-Type: application/json' \
    --data "{\"version\":\"$R_VERSION\",\"os\":\"linux\",\"arch\":\"$arch\"}" \
    https://api.pyo.dev/v1/installs >/dev/null 2>&1 || true
}

# The agent on this machine's cluster, if it's connected, moves along too
# (what Pyo's dashboard updates do for every cluster).
update_local_agent() {
  if kc -n "$NS" get deployment pyo-agent >/dev/null 2>&1; then
    kc -n "$NS" set image deployment/pyo-agent "agent=$R_AGENT" >/dev/null
    quietly "Updating this machine's agent…" kc -n "$NS" rollout status deployment/pyo-agent --timeout=180s ||
      echo "The agent is still starting. It shows in the dashboard when it's back."
  fi
}

# Pyo's dashboard answers on this machine.
dashboard_up() { curl -fsS -m 5 -o /dev/null http://127.0.0.1:8000/api/v1/meta 2>/dev/null; }

# ── Where it is ─────────────────────────────────────────────────────────────

# This machine's bootstrap key (bootstrap_key), for Pyo's /install calls.
host_key() { cat /etc/pyo/bootstrap-key 2>/dev/null; }

# Whether the first admin still has to sign in: "yes", "no" or "unknown"
# (Pyo didn't answer, or is older than this question).
first_sign_in_pending() {
  local key r
  key=$(host_key) && [ -n "$key" ] || { echo unknown; return 0; }
  r=$(curl -fsS -m 10 "http://127.0.0.1:8000/install/first-sign-in?key=$key" 2>/dev/null) || r=""
  case "$r" in
    *'"pending":true'*) echo yes ;;
    *'"pending":false'*) echo no ;;
    *) echo unknown ;;
  esac
}

# The first sign-in: on a fresh install (ADMIN_PASSWORD, from first_sign_in;
# an install that stopped halfway is one too), and on any later run while
# nobody has signed in with it yet (the pyo-admin Secret has it). The Secret
# stays after the first sign-in, so only Pyo can tell. When it can't, the
# email and where the password is, never the password itself.
print_sign_in() {
  local pending=""
  if [ -z "${ADMIN_PASSWORD:-}" ]; then
    pending=$(first_sign_in_pending)
    [ "$pending" != yes ] ||
      ADMIN_PASSWORD=$(kc -n "$NS" get secret pyo-admin -o jsonpath='{.data.password}' 2>/dev/null | base64 -d 2>/dev/null || true)
  fi
  echo
  if [ -n "${ADMIN_PASSWORD:-}" ]; then
    echo "Sign in with this login, then choose your own email and password:"
    echo "  Email:    admin@pyo.local"
    echo "  Password: $ADMIN_PASSWORD"
    echo "It stops working once you do. Lost it before signing in? On this machine:"
    echo "  sudo k3s kubectl -n $NS get secret pyo-admin -o jsonpath='{.data.password}' | base64 -d"
    return 0
  fi
  if [ "$pending" = unknown ] && kc -n "$NS" get secret pyo-admin >/dev/null 2>&1; then
    echo "Not signed in yet? Use the email admin@pyo.local and the password from:"
    echo "  sudo k3s kubectl -n $NS get secret pyo-admin -o jsonpath='{.data.password}' | base64 -d"
  fi
  echo "To update later, run the same command again."
}

# ── Uninstalling ────────────────────────────────────────────────────────────

# What runs in this k3s that's neither Pyo's nor k3s's own, into FOREIGN
# (each keeps a k3s Pyo installed: --uninstall removes it only without
# them, or with --remove-k3s) and EXTRA (other namespaces with nothing in
# them, which don't). Fails when the cluster can't be read.
#   Pyo's: namespaces pyo-system, pyo-gateway, pyo-waf, pyo-databases,
#   pyo-db-*, and those labelled pyo.dev/managed=true (projects),
#   pyo.dev/bundle (system bundles), app.kubernetes.io/managed-by=pyo or
#   part-of=pyo; Pyo's HelmCharts (managed-by=pyo, in kube-system) and the
#   namespaces they install into (pyo-gateway, cert-manager); Pyo's objects
#   in kube-system (managed-by=pyo).
#   k3s's: namespaces default, kube-system, kube-public, kube-node-lease,
#   and what k3s ships in kube-system: CoreDNS, local-path-provisioner,
#   metrics-server, ServiceLB's svclb-* and its charts (traefik,
#   traefik-crd, gateway-api-crd); the charts' helm-install jobs.
#   Anything else is foreign: another namespace with workloads, volume
#   claims or Helm releases; another HelmChart or Helm release; other
#   workloads, bare pods or claims in k3s's namespaces; and volumes not
#   claimed in Pyo's namespaces.
k3s_foreign() {
  local pyo_ns=" pyo-system pyo-gateway pyo-waf pyo-databases "
  local k3s_ns=" default kube-system kube-public kube-node-lease "
  local k3s_charts=" traefik traefik-crd gateway-api-crd "
  local k3s_deploys=" coredns local-path-provisioner metrics-server traefik "
  local pyo_charts=" " out ns name a b c d claims=" "
  local -A busy=()
  FOREIGN=() EXTRA=()
  local cols=()
  # Rows of `kc get "$@"`, cols' fields joined by "|".
  k3s_rows() { kc get "$@" -o jsonpath="{range .items[*]}$(IFS='|'; echo "${cols[*]}"){\"\\n\"}{end}" 2>/dev/null; }
  kc get namespace kube-system >/dev/null 2>&1 || return 1

  # Pyo's charts, and the namespaces they install into; other charts.
  cols=('{.metadata.namespace}' '{.metadata.name}' '{.metadata.labels.app\.kubernetes\.io/managed-by}' '{.spec.targetNamespace}')
  if kc get crd helmcharts.helm.cattle.io >/dev/null 2>&1; then
    out=$(k3s_rows helmcharts.helm.cattle.io -A) || return 1
    while IFS='|' read -r ns name a b; do
      [ -n "$name" ] || continue
      if [ "$ns" = kube-system ] && [ "$a" = pyo ]; then
        pyo_charts+="$name "
        [ -z "$b" ] || pyo_ns+="$b "
      elif [ "$ns" = kube-system ] && [[ $k3s_charts == *" $name "* ]]; then
        :
      else
        FOREIGN+=("HelmChart $ns/$name")
      fi
    done <<<"$out"
  fi

  # Namespaces.
  cols=('{.metadata.name}' '{.metadata.labels.pyo\.dev/managed}' '{.metadata.labels.pyo\.dev/bundle}' '{.metadata.labels.app\.kubernetes\.io/managed-by}' '{.metadata.labels.app\.kubernetes\.io/part-of}')
  out=$(k3s_rows namespaces) || return 1
  while IFS='|' read -r name a b c d; do
    [ -n "$name" ] || continue
    if [[ $pyo_ns == *" $name "* || $name == pyo-db-* || $a == true || -n $b || $c == pyo || $d == pyo ]]; then
      pyo_ns+="$name "
    elif [[ $k3s_ns != *" $name "* ]]; then
      busy[$name]=""
    fi
  done <<<"$out"
  # Another namespace counts with something in it; in k3s's, each object.
  k3s_thing() { # namespace, what's in it ("Kind name")
    if [[ $pyo_ns == *" $1 "* ]]; then
      return 0
    elif [[ $k3s_ns == *" $1 "* ]]; then
      FOREIGN+=("${2% *} $1/${2##* }")
    else
      busy[$1]=1
    fi
  }

  # Helm releases (Helm 3 keeps them in Secrets or ConfigMaps, owner=helm).
  cols=('{.metadata.namespace}' '{.metadata.labels.name}')
  out=$(k3s_rows secrets,configmaps -A -l owner=helm) || return 1
  while IFS='|' read -r ns name; do
    [ -n "$name" ] || continue
    [[ $pyo_ns != *" $ns "* ]] || continue
    if [ "$ns" = kube-system ] && [[ $k3s_charts$pyo_charts == *" $name "* ]]; then continue; fi
    FOREIGN+=("Helm release $name (namespace $ns)")
    [[ $k3s_ns == *" $ns "* ]] || busy[$ns]=1
  done < <(sort -u <<<"$out")

  # Workloads. Pods only bare ones (others are their controllers'), in
  # k3s's namespaces; elsewhere any.
  cols=('{.kind}' '{.metadata.namespace}' '{.metadata.name}' '{.metadata.labels.app\.kubernetes\.io/managed-by}' '{.metadata.labels.helmcharts\.helm\.cattle\.io/chart}' '{.metadata.ownerReferences[0].kind}')
  out=$(k3s_rows deployments,statefulsets,daemonsets,replicasets,jobs,cronjobs,pods -A) || return 1
  local kind owner
  while IFS='|' read -r kind ns name a b owner; do
    [ -n "$name" ] || continue
    if [[ $k3s_ns == *" $ns "* ]]; then
      case "$kind/$owner" in
        ReplicaSet/Deployment | Job/CronJob | Pod/ReplicaSet | Pod/DaemonSet | Pod/StatefulSet | Pod/Job) continue ;;
      esac
      if [ "$ns" = kube-system ]; then
        [ "$a" != pyo ] || continue
        case "$kind" in
          Deployment) [[ $k3s_deploys != *" $name "* ]] || continue ;;
          DaemonSet) [[ $name != svclb-* ]] || continue ;;
          # A chart's (k3s's, Pyo's, or another, listed with the charts).
          Job) [ -z "$b" ] || continue ;;
        esac
      fi
    fi
    k3s_thing "$ns" "$kind $name"
  done <<<"$out"

  # Volume claims, and volumes not claimed by one of them in Pyo's
  # namespaces (or still claimed, by a claim listed already).
  cols=('{.metadata.namespace}' '{.metadata.name}')
  out=$(k3s_rows persistentvolumeclaims -A) || return 1
  while IFS='|' read -r ns name; do
    [ -n "$name" ] || continue
    claims+="$ns/$name "
    k3s_thing "$ns" "PersistentVolumeClaim $name"
  done <<<"$out"
  cols=('{.metadata.name}' '{.spec.claimRef.namespace}' '{.spec.claimRef.name}')
  out=$(k3s_rows persistentvolumes) || return 1
  while IFS='|' read -r name ns a; do
    [ -n "$name" ] || continue
    [[ -z $ns || $pyo_ns != *" $ns "* ]] || continue
    [[ -z $ns || $claims != *" $ns/$a "* ]] || continue
    FOREIGN+=("PersistentVolume $name${ns:+ (claim $ns/$a)}")
  done <<<"$out"

  # Other namespaces: with something in them first, then the empty ones.
  local list=()
  for name in "${!busy[@]}"; do [ -z "${busy[$name]}" ] || list+=("namespace $name"); done
  for name in "${!busy[@]}"; do [ -n "${busy[$name]}" ] || EXTRA+=("namespace $name"); done
  mapfile -t list < <(printf '%s\n' "${list[@]}" | sort -u | grep . || true)
  mapfile -t EXTRA < <(printf '%s\n' "${EXTRA[@]}" | sort -u | grep . || true)
  FOREIGN=("${list[@]}" "${FOREIGN[@]}")
}

# install.sh --uninstall: Pyo's host timers and scripts, then k3s with
# everything in it, through k3s's own k3s-uninstall.sh, when this script
# installed k3s (its marker, /etc/pyo/k3s-installed) and nothing but Pyo
# and k3s's own runs in it (k3s_foreign), or with --remove-k3s. The volumes'
# data (k3s's local-path storage: Pyo's own database and registry, your
# volumes and databases) is moved aside first, unless --purge. A k3s that
# was here before Pyo, or that runs other things too, stays: Pyo is removed
# from it with connect.sh --uninstall, from the same release as this script
# (the latest, or --version's); the marker stays then, for --remove-k3s.
# Unless --purge, with --keep-data: Pyo's own volumes keep their data too.
# Then Pyo's AppArmor profile and firewall rules, only when k3s went (k3s
# needs them), and only Pyo's own (its header, its rule comments).
# Tailscale is left as it is: this machine stays on your tailnet.
uninstall_pyo() {
  local K3S_MARK=/etc/pyo/k3s-installed STORAGE=/var/lib/rancher/k3s/storage
  local APPARMOR=/etc/apparmor.d/cri-containerd.apparmor.d
  local KEPT_DIR
  KEPT_DIR="/var/lib/pyo-kept/$(date -u +%Y%m%d-%H%M%S)"
  exec 4>&1
  act() {
    if [ -n "$DRY_RUN" ]; then echo "  would run: $*" >&4; else "$@"; fi
  }
  local has_k3s="" k3s_ours="" apparmor_ours="" units="" others=()
  command -v k3s >/dev/null && has_k3s=1
  [ -f "$K3S_MARK" ] && k3s_ours=1
  [ -n "$has_k3s" ] || k3s_ours=""
  if [ -n "$REMOVE_K3S" ] && [ -n "$has_k3s" ] && [ -z "$k3s_ours" ]; then
    fail "--remove-k3s only removes a k3s that Pyo installed. This one was here before Pyo."
  fi
  [ -z "$KEEP_K3S" ] || k3s_ours=""
  [ -f "$APPARMOR" ] && head -n1 "$APPARMOR" | grep -q '^# Managed by Pyo' && apparmor_ours=1
  [ -e /etc/systemd/system/pyo-connect.timer ] || [ -e /etc/systemd/system/pyo-tailnet.timer ] || [ -d /usr/local/lib/pyo ] || [ -d /etc/pyo ] && units=1
  if [ -z "$units" ] && ! { [ -n "$has_k3s" ] && kc get namespace pyo-system >/dev/null 2>&1; }; then
    say "Pyo isn't installed on this machine. Nothing to do"
    return 0
  fi
  # A k3s Pyo installed goes only with nothing else in it (or --remove-k3s).
  local foreign_kept="" checked=""
  FOREIGN=() EXTRA=()
  if [ -n "$k3s_ours" ]; then
    if k3s_foreign; then
      checked=1
    else
      FOREIGN=() EXTRA=()
      [ -n "$REMOVE_K3S" ] || fail "Can't reach k3s (k3s kubectl) to check whether anything besides Pyo runs in it, so it isn't removed. Start it (sudo systemctl start k3s) and run this again. Or run again with --remove-k3s to remove it with everything in it, or with --keep-k3s to keep it."
    fi
    if [ "${#FOREIGN[@]}" -gt 0 ] && [ -z "$REMOVE_K3S" ]; then
      k3s_ours="" foreign_kept=1
    fi
  fi
  # Other servers that joined this cluster: they're uninstalled on their own.
  if [ -n "$k3s_ours" ]; then
    mapfile -t others < <(kc get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | grep -vx "$(hostname)" || true)
  fi
  local keep_storage=""
  [ -n "$k3s_ours" ] && [ -z "$PURGE" ] && [ -n "$(ls -A "$STORAGE" 2>/dev/null)" ] && keep_storage=1

  echo "This removes Pyo from this machine:"
  [ -z "$units" ] || echo "  - Pyo's timers and scripts (pyo-connect, pyo-tailnet, pyo-reset-login; /usr/local/lib/pyo, /etc/pyo)"
  if [ -n "$k3s_ours" ]; then
    echo "  - k3s, which Pyo installed, with everything in it: Pyo, its database and registry,"
    echo "    and ALL YOUR SERVICES, DATABASES AND VOLUMES (with k3s's k3s-uninstall.sh)"
    if [ "${#FOREIGN[@]}" -gt 0 ]; then
      echo "    AND THESE, WHICH AREN'T PYO'S (--remove-k3s):"
      printf '      %s\n' "${FOREIGN[@]}"
    elif [ -n "$REMOVE_K3S" ] && [ -z "$checked" ]; then
      echo "    k3s can't be reached, so what else is in it wasn't checked (--remove-k3s)."
    fi
    [ "${#EXTRA[@]}" -eq 0 ] || echo "    Also its empty namespaces that aren't Pyo's: ${EXTRA[*]#namespace }"
    if [ -n "$keep_storage" ]; then
      echo "    The volumes' files ($STORAGE) are moved to $KEPT_DIR first."
    else
      echo "    Their data is deleted too$([ -n "$PURGE" ] && echo " (--purge)")."
    fi
    [ -z "$apparmor_ours" ] || echo "  - Pyo's AppArmor profile ($APPARMOR)"
    echo "  - firewall rules Pyo added (ufw, commented \"Pyo ingress\" or \"k3s over Tailscale\")"
  elif [ -n "$has_k3s" ]; then
    echo "  - Pyo itself and everything it put into k3s (connect.sh --uninstall)"
    if [ -n "$PURGE" ]; then
      echo "    with ALL YOUR SERVICES, DATABASES AND VOLUMES (--purge)"
    else
      echo "    Your projects' namespaces, databases and volumes stay in k3s. The volumes of what's"
      echo "    removed (Pyo's own database, registry and backup store) keep their data on disk."
    fi
    if [ -n "$foreign_kept" ]; then
      echo "    k3s itself stays, though Pyo installed it, because these in it aren't Pyo's:"
      printf '      %s\n' "${FOREIGN[@]}"
      [ "${#EXTRA[@]}" -eq 0 ] || echo "    Empty namespaces that aren't Pyo's (these alone don't keep k3s): ${EXTRA[*]#namespace }"
      echo "    (--remove-k3s removes k3s with them.)"
    else
      echo "    k3s itself stays: $([ -n "$KEEP_K3S" ] && echo "--keep-k3s" || echo "it was here before Pyo")."
    fi
  fi
  echo "Tailscale stays as it is: this machine stays on your tailnet."
  echo
  echo "Back up first: https://pyo.dev/docs/uninstall"
  echo
  if [ -n "$DRY_RUN" ]; then
    echo "Dry run: nothing is changed."
  elif [ -z "$YES" ]; then
    local answer=""
    if [ -r /dev/tty ] && { exec 3</dev/tty; } 2>/dev/null; then
      read -r -p "Type yes to uninstall Pyo: " answer <&3 || true
      exec 3<&-
    else
      fail "Run again with --yes to uninstall without asking."
    fi
    [ "$answer" = yes ] || fail "Nothing was changed."
  fi

  # 1. The host's timers first, so nothing connects this machine again.
  say "Removing Pyo's timers and scripts"
  local u
  for u in pyo-connect.timer pyo-connect.service pyo-tailnet.timer pyo-tailnet.service; do
    if [ -e "/etc/systemd/system/$u" ]; then
      act systemctl disable --now "$u" >/dev/null 2>&1 || true
      act rm -f "/etc/systemd/system/$u"
    fi
  done
  act systemctl daemon-reload
  act rm -rf /usr/local/lib/pyo
  act rm -f /usr/local/bin/pyo-reset-login

  # 2. k3s, or Pyo out of a k3s that stays.
  if [ -n "$k3s_ours" ]; then
    [ -x /usr/local/bin/k3s-uninstall.sh ] || fail "There's no /usr/local/bin/k3s-uninstall.sh, so k3s can't be removed here. Run again with --keep-k3s to keep it."
    if [ -n "$keep_storage" ]; then
      # Stopped first, so the files are as the services left them.
      say "Stopping k3s and moving the volumes' files to $KEPT_DIR"
      act /usr/local/bin/k3s-killall.sh >/dev/null 2>&1 || true
      act mkdir -p "$KEPT_DIR"
      act mv "$STORAGE" "$KEPT_DIR/storage"
    fi
    say "Uninstalling k3s"
    act /usr/local/bin/k3s-uninstall.sh >/dev/null 2>&1
    # What small_machine set up for it.
    act rm -f /etc/systemd/system/k3s.service.d/pyo-memory.conf /run/pyo-k3s.env /etc/rancher/k3s/config.yaml.d/50-pyo-memory.yaml /etc/rancher/k3s/config.yaml.d/40-pyo-tailnet.yaml
    if [ -e /etc/systemd/system/pyo-zram.service ]; then
      act systemctl disable --now pyo-zram.service >/dev/null 2>&1 || true
      act rm -f /etc/systemd/system/pyo-zram.service
    fi
    act rm -f /usr/local/lib/pyo-memory.sh
    act systemctl daemon-reload
    if [ -n "$apparmor_ours" ]; then
      act apparmor_parser -R "$APPARMOR" >/dev/null 2>&1 || true
      act rm -f "$APPARMOR"
    fi
    if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q "Status: active"; then
      local n
      # Highest number first: deleting one renumbers those after it.
      for n in $(ufw status numbered | grep -E '# (Pyo ingress|k3s over Tailscale)$' | sed -n 's/^\[ *\([0-9]*\)\].*/\1/p' | sort -rn); do
        act ufw --force delete "$n" >/dev/null
      done
    fi
  elif [ -n "$has_k3s" ]; then
    local script
    script=$(mktemp)
    fetch_connect "$script" || { rm -f "$script"; fail "Couldn't get connect.sh to remove Pyo from k3s."; }
    local args=(--uninstall --with-pyo --yes)
    [ -n "$PURGE" ] || args+=(--keep-data)
    [ -z "$DRY_RUN" ] || args+=(--dry-run)
    env -u KUBECONFIG bash "$script" "${args[@]}" || { rm -f "$script"; fail "Removing Pyo from k3s didn't finish (see above). Run this again."; }
    rm -f "$script"
  fi
  if [ -n "$foreign_kept" ]; then
    # The marker stays, so --remove-k3s can still remove k3s later.
    act find /etc/pyo -mindepth 1 -maxdepth 1 ! -name k3s-installed -exec rm -rf {} +
  else
    act rm -rf /etc/pyo
  fi

  echo
  if [ -n "$foreign_kept" ]; then
    local list
    list=$(printf ', %s' "${FOREIGN[@]}")
    echo "These aren't Pyo's: ${list#, }"
    echo "k3s $([ -n "$DRY_RUN" ] && echo "would be" || echo was) kept. Run again with --remove-k3s to remove it anyway."
  fi
  if [ -n "$DRY_RUN" ]; then
    echo "Dry run done: nothing was changed."
    return 0
  fi
  ok_line "Pyo is uninstalled"
  if [ -n "$keep_storage" ]; then
    echo "Kept the volumes' files (Pyo's own database, registry and your volumes and databases) in:"
    echo "  $KEPT_DIR/storage"
    echo "Delete them when you no longer need them: sudo rm -rf $KEPT_DIR"
  fi
  if [ "${#others[@]}" -gt 0 ]; then
    echo "Other servers of this cluster: ${others[*]}. On each, run:"
    echo "  sudo /usr/local/bin/k3s-agent-uninstall.sh"
  fi
  echo "This machine is still on your tailnet. You can remove it in the Tailscale admin console."
}

cleanup() { [ -z "${WORK:-}" ] || rm -rf "$WORK"; }

main() {
  # Again here: after sudo, only the functions and SETTINGS came along.
  set -euo pipefail
  defaults
  parse_args "$@"
  become_root "$@"
  if [ -n "$UNINSTALL" ]; then
    uninstall_pyo
    return 0
  fi
  check_system
  WORK=$(mktemp -d)
  trap cleanup EXIT
  # The release first: a bad --version fails before anything changes.
  fetch_release
  local fresh=1 cur=""
  if installed; then
    fresh=""
    cur=$(current_version)
    if ! check_update "$cur"; then
      install_k3s
      # Pyo's scripts on this machine, like pyo-reset-login, as this
      # release has them.
      host_units
      pyo_addresses 0 "Pyo $R_VERSION is already installed. Nothing to update"
      print_sign_in
      return 0
    fi
    say "Updating Pyo ${cur:-(unknown version)} to $R_VERSION"
  else
    check_ports
    say "Installing Pyo $R_VERSION"
  fi
  install_k3s
  host_units
  ADMIN_PASSWORD=""
  run_upgrader "$fresh"
  STARTING=""
  wait_until "Starting the dashboard…" 300 dashboard_up || STARTING=1
  if [ -n "$fresh" ]; then
    # The first sign-in is printed below.
    count_install
  else
    ADMIN_PASSWORD=""
    update_local_agent
  fi
  # This machine as a cluster: its gateway serves Pyo over HTTPS on the
  # public IP, which the addresses wait for. After an update, its cluster
  # is refreshed for the new release, as the command under "Refresh this
  # cluster" does.
  if [ -n "$fresh" ]; then
    quietly "Connecting this machine to Pyo…" /usr/local/lib/pyo/connect-this-machine.sh ||
      echo "Couldn't connect this machine to Pyo yet. Pyo tries again every minute."
  else
    quietly "Refreshing this server…" /usr/local/lib/pyo/connect-this-machine.sh refresh ||
      echo "Couldn't refresh this server yet. Pyo tries again in an hour."
  fi
  if [ -n "$fresh" ]; then
    # No HTTPS address on the public IP to wait for while 80 and 443 are taken.
    pyo_addresses "$([ -n "${WEB_PORTS:-}" ] && echo 0 || echo 300)" "Pyo $R_VERSION is installed"
  else
    pyo_addresses 300 "Pyo is updated to $R_VERSION"
  fi
  print_sign_in
}

main "$@"
