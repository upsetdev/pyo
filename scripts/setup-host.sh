#!/usr/bin/env bash
# One-time host setup for developing Pyo on this machine (installing it is
# deploy/install.sh). Everything Pyo runs (Postgres, the API, the dashboard,
# the agent, BuildKit and the image registry) runs inside k3s; the host only
# needs k3s itself, plus build tools for `cargo test`. Needs root:
#
#   sudo bash scripts/setup-host.sh
#
# Host ports once Pyo is running:
#   80 / 443  Pyo Gateway (services' HTTP and HTTPS)
#   8443      Pyo dashboard over HTTPS (the gateway; tailnet sources only)
#   5443      Pyo registry for teams' own images (HTTPS; tailnet only)
#   8000      Pyo dashboard and API, plain HTTP: localhost and the tailnet
#   6443      k3s API server
#   5005      Pyo image registry: 127.0.0.1 for this server; on the tailnet IP
#             only its door, which wants a login
#
# Undo: /usr/local/bin/k3s-uninstall.sh
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "Run with sudo: sudo bash $0" >&2
  exit 1
fi

# Pyo Gateway needs 80 and 443, the dashboard 8000 and the registry 5005.
# Stop here rather than install k3s with them unable to bind.
# (A k3s that's already running holds 80 and 443 itself.)
BUSY=""
command -v k3s >/dev/null || BUSY=$(ss -ltnH '( sport = :80 or sport = :443 or sport = :8000 or sport = :5005 )' || true)
if [ -n "$BUSY" ]; then
  echo "These ports are in use:" >&2
  echo "$BUSY" >&2
  docker ps --format '  {{.Names}}  {{.Ports}}' 2>/dev/null | grep -E ':(80|443|8000|5005)->' >&2 || true
  echo "Stop whatever holds them, then run this script again." >&2
  exit 1
fi

# ── 1. Build tools for developing Pyo on this machine ────────────────────
# Lets `cargo check` and `cargo test` run locally. Images are still built by
# BuildKit inside k3s.
apt-get update
apt-get install -y build-essential pkg-config

# ── 2. k3s ─────────────────────────────────────────────────────────────────
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
  local env before zram_was=""
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
  # Said only when it turns on here, not when it's on already.
  [ ! -f /run/pyo-zram ] || zram_was=1
  systemctl enable --now pyo-zram.service >/dev/null 2>&1 || echo "Couldn't set up compressed swap (zram); going on without it." >&2
  env=$(/usr/local/lib/pyo-memory.sh k3s-env)
  [ -z "$env" ] || echo "A small machine: k3s gets $env."
  /usr/local/lib/pyo-memory.sh k3s-config "$K3S_MEMORY_CONFIG" || true
  [ -n "$zram_was" ] || [ ! -f /run/pyo-zram ] || echo "Turned on compressed swap in memory (zram)."
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
# The k3s Pyo pins (deploy/k3s.env), downloaded and checked first.
K3S_DIR=$(mktemp -d)
trap 'rm -rf "$K3S_DIR"' EXIT
if ! command -v k3s >/dev/null; then
  # shellcheck source=deploy/k3s.env
  . "$(dirname "${BASH_SOURCE[0]}")/../deploy/k3s.env"
  k3s_fetch "$K3S_DIR" "$PYO_K3S_VERSION"
fi
mkdir -p /etc/rancher/k3s
cat > /etc/rancher/k3s/config.yaml <<'CONFIG'
# Lets the dev user read /etc/rancher/k3s/k3s.yaml without sudo.
write-kubeconfig-mode: "0644"
# Secrets are encrypted in the datastore with secretbox. On a k3s that was
# already running, also run: k3s secrets-encrypt reencrypt
secrets-encryption: true
secrets-encryption-provider: secretbox
CONFIG
# The Kubernetes API's certificate also names this machine's MagicDNS name,
# when it's on a tailnet with MagicDNS.
TS_NAME=$(tailscale status --json 2>/dev/null | sed -n '/"Self"/,/}/s/^ *"DNSName": *"\([^"]*\)",*$/\1/p' | head -n1 || true)
TS_NAME=${TS_NAME%.}
[ -z "$TS_NAME" ] || printf 'tls-san:\n  - "%s"\n' "$TS_NAME" >> /etc/rancher/k3s/config.yaml
# Pyo's registry runs in the cluster on 127.0.0.1:5005 over plain HTTP.
# Images built by Pyo are pushed there and pulled by containerd from there.
cat > /etc/rancher/k3s/registries.yaml <<'REGISTRIES'
mirrors:
  "127.0.0.1:5005":
    endpoint:
      - "http://127.0.0.1:5005"
REGISTRIES
if ! command -v k3s >/dev/null; then
  small_machine
  # Pyo Gateway takes 80/443, not k3s's Traefik.
  k3s_install "$K3S_DIR" server --disable traefik
  # So install.sh --uninstall removes the k3s it installed, as for an install.
  install -d -m 700 /etc/pyo
  echo "k3s was installed by Pyo's scripts/setup-host.sh on $(date -u +%F)." > /etc/pyo/k3s-installed
else
  k3s_memory_refresh later
  systemctl restart k3s
fi

echo
echo "Done. Try: kubectl get nodes"
