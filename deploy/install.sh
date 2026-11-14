#!/usr/bin/env bash
# Installs Pyo on this machine, or updates it to a newer release:
#
#   curl -fsSL https://pyo.dev/install.sh | bash
#   curl -fsSL https://pyo.dev/install.sh | bash -s -- --version 0.1.0
#
# Options (or environment variables):
#   --version X   a release instead of the latest (PYO_VERSION)
#   --yes         don't ask before an update that has notices (PYO_YES=1)
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
  WORK=""
  UNINSTALL="" PURGE="" KEEP_K3S="" REMOVE_K3S="" DRY_RUN=""
}

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
fail() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

kc() { k3s kubectl "$@"; }

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
        sed -n '2,19p' "${BASH_SOURCE[0]:-/dev/null}" 2>/dev/null || true
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

# Root, or again with sudo: the whole script travels as its functions, so this
# works when it came through a pipe.
become_root() {
  [ "$(id -u)" = 0 ] && return 0
  command -v sudo >/dev/null || fail "Run this as root (there's no sudo)."
  say "Pyo needs root. Asking sudo"
  # shellcheck disable=SC2016 # expanded by the new shell
  exec sudo env PYO_VERSION="$VERSION" PYO_YES="$YES" PYO_UPDATE_SOURCE="$SOURCE" \
    bash -c "$(declare -f); main \"\$@\"" install.sh "$@"
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

# Ports Pyo takes. With k3s already running, 80 and 443 are its traffic
# proxy's.
check_ports() {
  local ports=(8000 5005)
  command -v k3s >/dev/null || ports+=(80 443 6443)
  local p busy=""
  for p in "${ports[@]}"; do
    if ss -ltnH "( sport = :$p )" 2>/dev/null | grep -q .; then busy="$busy $p"; fi
  done
  if [ -n "$busy" ]; then
    echo "These ports are in use:$busy" >&2
    ss -ltnpH 2>/dev/null | grep -E ":($(echo "$busy" | xargs | tr ' ' '|'))\b" >&2 || true
    fail "Pyo needs them (80/443 for your services, 8000 for the dashboard, 5005 for its registry, 6443 for k3s). Stop what holds them and run this again."
  fi
}

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
    echo "Note: $f isn't Pyo's, so it's left alone. On this kernel, containers may hang." >&2
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
  echo "Applied Pyo's AppArmor workaround for kernel $kv."
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
k3s_get() { curl -fsSL --proto '=https' --retry 3 --max-time 600 -o "$2" "$1" || k3s_fail "Couldn't download $1."; }
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
# >>> pyo:small-machine (the same in deploy/install.sh, connect.sh and scripts/setup-host.sh)
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
# 768 MiB under 8 GB, 1 GiB from there.
K3S_MEMORY_CONFIG=/etc/rancher/k3s/config.yaml.d/50-pyo-memory.yaml
K3S_MEMORY_CHANGED=""
small_machine() {
  local env before
  # What k3s runs with now: kubelet's reservations and its GOMEMLIMIT.
  before=$(cat "$K3S_MEMORY_CONFIG" /run/pyo-k3s.env 2>/dev/null || true)
  mkdir -p /etc/systemd/system/k3s.service.d
  cat > /usr/local/lib/pyo-memory.sh <<'MEMORY'
#!/bin/sh
# Managed by Pyo: k3s's GOMEMLIMIT, kubelet's reservations and compressed
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
case "$1" in
  k3s-env)
    # For k3s's EnvironmentFile: GOMEMLIMIT under 3.5 GB, else nothing.
    if [ "${mem:-0}" -gt 0 ] && [ "$mem" -lt 3584 ]; then
      echo "GOMEMLIMIT=$(gomemlimit)MiB"
    fi
    ;;
  k3s-config)
    # k3s's config drop-in ($2): kubelet's reservations; none (no file)
    # when the memory can't be read.
    [ -n "$2" ] || exit 2
    if [ "${mem:-0}" -gt 0 ]; then
      set -- "$2" $(reserved)
      mkdir -p "$(dirname "$1")"
      printf '%s\n' "# Managed by Pyo: what kubelet sets aside, from this machine's memory at each start." \
        'kubelet-arg+:' "  - \"kube-reserved=memory=$2Mi\"" "  - \"system-reserved=memory=$3Mi\"" > "$1.new" && mv -f "$1.new" "$1"
    else
      rm -f "$2"
    fi
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
    'EnvironmentFile=-/run/pyo-k3s.env' > /etc/systemd/system/k3s.service.d/pyo-memory.conf
  printf '%s\n' '[Unit]' 'Description=Pyo: compressed swap in memory (zram) on a small machine' 'Before=k3s.service' \
    '[Service]' 'Type=oneshot' 'RemainAfterExit=yes' \
    'ExecStart=/usr/local/lib/pyo-memory.sh zram-start' 'ExecStop=/usr/local/lib/pyo-memory.sh zram-stop' \
    '[Install]' 'WantedBy=multi-user.target' > /etc/systemd/system/pyo-zram.service
  systemctl daemon-reload
  systemctl enable --now pyo-zram.service >/dev/null 2>&1 || echo "Couldn't set up compressed swap (zram); going on without it." >&2
  env=$(/usr/local/lib/pyo-memory.sh k3s-env)
  [ -z "$env" ] || echo "A small machine: k3s gets $env."
  /usr/local/lib/pyo-memory.sh k3s-config "$K3S_MEMORY_CONFIG" || true
  [ ! -f /run/pyo-zram ] || echo "Turned on compressed swap in memory (zram)."
  [ "$before" = "$(cat "$K3S_MEMORY_CONFIG" 2>/dev/null; [ -z "$env" ] || echo "$env")" ] || K3S_MEMORY_CHANGED=1
}
# The same on a k3s that's already installed (k3s.service). k3s uses what
# changed once it restarts: it restarts here, unless $1 is "later" (the
# caller restarts it, when K3S_MEMORY_CHANGED is set). Containers keep
# running while k3s restarts.
k3s_memory_refresh() {
  systemctl cat k3s.service >/dev/null 2>&1 || return 0
  K3S_MEMORY_CHANGED=""
  small_machine
  [ -n "$K3S_MEMORY_CHANGED" ] || return 0
  echo "k3s restarts briefly to apply its memory reserve. Services keep running."
  [ "${1:-}" = later ] || systemctl restart k3s
}
# <<< pyo:small-machine

# A field of release.json's "k3s".
k3s_pin() { sed -n "/^  \"k3s\": {/,/^  }/s/^    \"$1\": \"\([^\"]*\)\",*\$/\1/p" "$WORK/release.json"; }

# k3s with Pyo's settings: Secrets encrypted with secretbox, and containerd
# pulling Pyo's own registry (127.0.0.1:5005) over HTTP. An existing k3s is
# used as it is, apart from its memory reserve (k3s_memory_refresh).
install_k3s() {
  if command -v k3s >/dev/null; then
    say "Using the k3s that's installed"
    k3s_memory_refresh
  else
    say "Installing k3s"
    # The release's k3s (release.json's "k3s", from deploy/k3s.env),
    # downloaded and checked before anything changes.
    PYO_K3S_VERSION=$(k3s_pin version)
    PYO_K3S_INSTALLER_SHA256=$(k3s_pin installerSha256)
    PYO_K3S_SHA256_AMD64=$(k3s_pin amd64Sha256)
    PYO_K3S_SHA256_ARM64=$(k3s_pin arm64Sha256)
    k3s_fetch "$WORK/k3s" "$PYO_K3S_VERSION"
    apparmor_workaround
    mkdir -p /etc/rancher/k3s
    cat > /etc/rancher/k3s/config.yaml <<'CONFIG'
# Secrets are encrypted in the datastore with secretbox (XSalsa20-Poly1305).
secrets-encryption: true
secrets-encryption-provider: secretbox
CONFIG
    # The Kubernetes API's certificate also names this machine's MagicDNS
    # name, when it's on a tailnet with MagicDNS.
    local ts_name
    ts_name=$(tailscale status --json 2>/dev/null | sed -n '/"Self"/,/}/s/^ *"DNSName": *"\([^"]*\)",*$/\1/p' | head -n1 || true)
    ts_name=${ts_name%.}
    [ -z "$ts_name" ] || printf 'tls-san:\n  - "%s"\n' "$ts_name" >> /etc/rancher/k3s/config.yaml
    cat > /etc/rancher/k3s/registries.yaml <<'REGISTRIES'
mirrors:
  "127.0.0.1:5005":
    endpoint:
      - "http://127.0.0.1:5005"
REGISTRIES
    small_machine
    # Pyo Gateway takes 80/443, not k3s's Traefik.
    k3s_install "$WORK/k3s" server --disable traefik
    # So --uninstall knows k3s is Pyo's to remove.
    install -d -m 700 /etc/pyo
    echo "k3s was installed by Pyo's install.sh on $(date -u +%F)." > /etc/pyo/k3s-installed
  fi
  local i
  for i in $(seq 90); do
    kc get nodes >/dev/null 2>&1 && return 0
    sleep 2
  done
  fail "k3s isn't answering. Check: sudo systemctl status k3s"
}

# ── The release ─────────────────────────────────────────────────────────────

# Only GitHub, or the configured mirror, and HTTPS unless the mirror isn't.
fetch() {
  local proto='=https'
  case "$SOURCE" in http://*) proto='=http,https' ;; esac
  curl -fsSL --proto "$proto" --max-time 60 -H 'Accept: application/vnd.github+json' "$@"
}

# The release this script installs: the latest, or --version's (GitHub's
# description of it, with its assets' download addresses).
release_meta() {
  if [ -n "$VERSION" ]; then
    fetch "$SOURCE/releases/tags/v$VERSION"
  else
    fetch "$SOURCE/releases/latest"
  fi
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
  [ -n "$asset" ] && fetch -o "$1" "$asset"
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
  say "Downloading cosign $COSIGN_VERSION to check the release's signature"
  curl -fsSL --proto '=https' --retry 3 --max-time 300 -o "$WORK/cosign" \
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
  fetch -o "$WORK/release.json.sigstore.json" "$bundle" || fail "Couldn't download $bundle."
  find_cosign
  say "Checking the release's signature"
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
  meta=$(release_meta 2>/dev/null) || {
    [ -n "$VERSION" ] && fail "There's no release $VERSION. See https://github.com/upsetdev/pyo/releases."
    fail "Couldn't reach $SOURCE to find the latest release."
  }
  asset=$(printf '%s' "$meta" | asset_url 'release\.json')
  [ -n "$asset" ] || fail "$what has no release.json, so it can't be installed with this script."
  fetch -o "$WORK/release.json" "$asset" || fail "Couldn't download $asset."
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
# The gateway's own Service never carries these ports (k3s's ServiceLB
# would publish them on every address and hand connections over from its
# pod, which the listeners' tailnet-only policy refuses): this one alone
# takes them, on the Tailscale address. kube-proxy hands a connection to
# the proxy on this machine unchanged (externalTrafficPolicy Local), so
# the proxy sees the device's own 100.x address.
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

# What this machine fills in for Pyo: its Tailscale IP, for other servers to
# reach the registry and builders to reach the API, and Pyo registry's
# address for teams' own images.
install_values() {
  TAILNET_IP=$(tailscale ip -4 2>/dev/null | head -n1 || true)
  REGISTRY_PUSH=${TAILNET_IP:+$TAILNET_IP:5005}
  TAILNET_API=${TAILNET_IP:+http://$TAILNET_IP:8000}
  tailnet_name
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

# The release's upgrader, as the agent runs it for dashboard updates (the
# objects match upgrade.rs's upgrader_rbac and run_upgrader).
run_upgrader() {
  local id name
  id="install-$(date +%s)"
  name="pyo-upgrade-$id"
  # The namespace as platform.yaml has it (the upgrader skips Namespaces).
  kc apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Namespace
metadata: { name: $NS, labels: { app.kubernetes.io/part-of: pyo } }
YAML
  first_sign_in
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
            - { name: PYO_INSTALL_TAILNET_IP, value: "$TAILNET_IP" }
            - { name: PYO_INSTALL_REGISTRY_PUSH, value: "$REGISTRY_PUSH" }
            - { name: PYO_INSTALL_TAILNET_API, value: "$TAILNET_API" }
            - { name: PYO_INSTALL_TAILNET_NAME, value: "$TAILNET_NAME" }
          volumeMounts: [{ name: release, mountPath: /release, readOnly: true }]
          resources: { requests: { cpu: 20m, memory: 32Mi }, limits: { memory: 256Mi } }
          securityContext: { runAsNonRoot: true, runAsUser: 65532, allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: [ALL] } }
      volumes: [{ name: release, configMap: { name: $name } }]
YAML
  say "Applying Pyo $R_VERSION and waiting until it's ready (a few minutes the first time)"
  local i st printed=0 pod
  for i in $(seq 380); do
    st=$(kc -n "$NS" get job "$name" -o jsonpath='{.status.succeeded}/{.status.failed}' 2>/dev/null || true)
    case "$st" in 1/* | */1) break ;; esac
    pod=$(kc -n "$NS" get pods -l "pyo/update-id=$id" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)
    # Its progress, as it goes.
    if [ -n "$pod" ]; then
      local lines
      lines=$(kc -n "$NS" logs "$pod" 2>/dev/null | grep -v pyoUpgradeOutcome || true)
      if [ -n "$lines" ]; then
        printf '%s\n' "$lines" | tail -n +$((printed + 1)) | sed 's/^/    /'
        printed=$(printf '%s\n' "$lines" | wc -l)
      fi
      local why
      why=$(kc -n "$NS" get pod "$pod" -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null || true)
      case "$why" in
        ErrImagePull | ImagePullBackOff | InvalidImageName)
          if [ "$i" -gt 60 ]; then
            kc -n "$NS" delete job "$name" --wait=false >/dev/null 2>&1 || true
            fail "This machine can't pull $R_AGENT ($why). Nothing was changed."
          fi
          ;;
      esac
    fi
    sleep 5
  done
  local outcome
  outcome=$(kc -n "$NS" logs "job/$name" 2>/dev/null | grep pyoUpgradeOutcome | tail -n1 || true)
  kc -n "$NS" delete configmap "$name" >/dev/null 2>&1 || true
  # Its last line says how it went; without it (logs gone), the Job's state.
  case "$outcome" in
    *'"ok":true'*) say "Pyo $R_VERSION is ready" ;;
    *'"ok":false'*) fail "$(printf '%s' "$outcome" | sed 's/.*"message":"\([^"]*\)".*/\1/')" ;;
    *)
      case "$st" in
        1/*) say "Pyo $R_VERSION is ready" ;;
        */1) fail "Updating to $R_VERSION failed. The version that was running keeps running. See: sudo k3s kubectl -n $NS logs job/$name" ;;
        *) fail "The upgrader didn't finish in time. Look at: sudo k3s kubectl -n $NS logs job/$name" ;;
      esac
      ;;
  esac
}

# The agent on this machine's cluster, if it's connected, moves along too
# (what Pyo's dashboard updates do for every cluster).
update_local_agent() {
  if kc -n "$NS" get deployment pyo-agent >/dev/null 2>&1; then
    say "Updating this cluster's agent"
    kc -n "$NS" set image deployment/pyo-agent "agent=$R_AGENT" >/dev/null
    kc -n "$NS" rollout status deployment/pyo-agent --timeout=180s >/dev/null || echo "The agent is still starting. It shows in the dashboard when it's back." >&2
  fi
}

# ── Where it is ─────────────────────────────────────────────────────────────

# Where Pyo is: on this machine, and over HTTPS on the tailnet when it
# has MagicDNS and HTTPS certificates. Nothing on public addresses.
print_addresses() {
  tailnet_name
  echo
  echo "Pyo is at:"
  [ -z "$TAILNET_NAME" ] || echo "  https://$TAILNET_NAME:8443  (from your tailnet, registry at $TAILNET_NAME:5443)"
  echo "  http://localhost:8000  (on this machine)"
  echo
  remote_access_note
}

print_sign_in() {
  echo
  if [ -n "${ADMIN_PASSWORD:-}" ]; then
    echo "Sign in with this login, then choose your own email and password:"
    echo "  Email:    admin@pyo.local"
    echo "  Password: $ADMIN_PASSWORD"
    echo "It stops working once you do. Lost it before signing in? On this machine:"
    echo "  sudo k3s kubectl -n $NS get secret pyo-admin -o jsonpath='{.data.password}' | base64 -d"
  else
    if kc -n "$NS" get secret pyo-admin >/dev/null 2>&1; then
      # An earlier run made the login (maybe one that stopped before
      # printing it). It works until the first sign-in sets your own.
      echo "First sign-in: admin@pyo.local, with the password from:"
      echo "  sudo k3s kubectl -n $NS get secret pyo-admin -o jsonpath='{.data.password}' | base64 -d"
    fi
    echo "To update later, run the same command again."
  fi
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
  [ -e /etc/systemd/system/pyo-connect.timer ] || [ -e /etc/systemd/system/pyo-tailnet-cert.timer ] || [ -d /usr/local/lib/pyo ] || [ -d /etc/pyo ] && units=1
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
  [ -z "$units" ] || echo "  - Pyo's timers and scripts (pyo-connect, pyo-tailnet-cert; /usr/local/lib/pyo, /etc/pyo)"
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
  for u in pyo-connect.timer pyo-connect.service pyo-tailnet-cert.timer pyo-tailnet-cert.service; do
    if [ -e "/etc/systemd/system/$u" ]; then
      act systemctl disable --now "$u" >/dev/null 2>&1 || true
      act rm -f "/etc/systemd/system/$u"
    fi
  done
  act systemctl daemon-reload
  act rm -rf /usr/local/lib/pyo

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
    act rm -f /etc/systemd/system/k3s.service.d/pyo-memory.conf /run/pyo-k3s.env /etc/rancher/k3s/config.yaml.d/50-pyo-memory.yaml
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
  say "Pyo is uninstalled"
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
  # Again here: after sudo, only the functions came along.
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
      say "Pyo $R_VERSION is already installed. Nothing to update"
      print_addresses
      print_sign_in
      return 0
    fi
    say "Updating Pyo ${cur:-(unknown version)} to $R_VERSION"
  else
    check_ports
    say "Installing Pyo $R_VERSION"
  fi
  install_k3s
  install_values
  host_units
  ADMIN_PASSWORD=""
  run_upgrader
  if [ -n "$fresh" ]; then
    : # The first sign-in is printed below.
  else
    ADMIN_PASSWORD=""
    update_local_agent
  fi
  # HTTPS on the tailnet before the address is shown, then this machine as
  # a cluster (its gateway serves it; waits for the first sign-up).
  [ -z "$TAILNET_NAME" ] || /usr/local/lib/pyo/tailnet-cert.sh || echo "Couldn't get the tailnet certificate yet. It's tried again daily (pyo-tailnet-cert.timer)." >&2
  /usr/local/lib/pyo/connect-this-machine.sh || echo "Couldn't connect this machine to Pyo yet (see above). It's tried again every minute (pyo-connect.timer)." >&2
  print_addresses
  print_sign_in
}

main "$@"
