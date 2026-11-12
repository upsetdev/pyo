#!/usr/bin/env bash
# Joins this machine to a Pyo cluster as a server (k3s agent).
# Pyo's "Add server" gives you the full command, e.g.:
#
#   curl -fsSL https://pyo.dev/join.sh | sudo bash -s -- \
#     --server pyo.tail1234.ts.net --token abc123.0123456789abcdef --name sg-1 \
#     --k3s v1.36.5+k3s1 [--tailscale-key tskey-auth-...] \
#     [--registry pyo.tail1234.ts.net:5005 --registry-token 9b1c...]
#
#   --server         the control plane's tailnet name or IP
#   --token          a join token (expires an hour after Pyo made it)
#   --name           this server's name in the cluster and on the tailnet
#   --k3s            the cluster's k3s version, installed here too
#   --tailscale-key  joins the tailnet when the machine isn't on it yet
#   --builder        a build-only server: only Pyo's builder runs on it
#   --registry       Pyo's registry door, for images Pyo builds
#   --registry-token the cluster's login there (pull only)
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

SERVER_IP="" K3S_TOKEN="" NODE_NAME="" K3S_VERSION="" TS_AUTHKEY="${TS_AUTHKEY:-}" BUILDER="" REGISTRY="" REGISTRY_TOKEN=""
while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER_IP="$2"; shift 2 ;;
    --token) K3S_TOKEN="$2"; shift 2 ;;
    --name) NODE_NAME="$2"; shift 2 ;;
    --k3s) K3S_VERSION="$2"; shift 2 ;;
    --tailscale-key) TS_AUTHKEY="$2"; shift 2 ;;
    --builder) BUILDER=1; shift ;;
    --registry) REGISTRY="$2"; shift 2 ;;
    --registry-token) REGISTRY_TOKEN="$2"; shift 2 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done
[ -n "$SERVER_IP" ] && [ -n "$K3S_TOKEN" ] && [ -n "$NODE_NAME" ] && [ -n "$K3S_VERSION" ] || { echo "Usage: join.sh --server HOST --token TOKEN --name NAME --k3s VERSION [--tailscale-key KEY]" >&2; exit 2; }
[[ $K3S_VERSION =~ ^v[0-9]+\.[0-9]+\.[0-9]+\+k3s[0-9]+$ ]] || { echo "--k3s takes the cluster's k3s version, like v1.37.2+k3s1; this is $K3S_VERSION." >&2; exit 2; }
[ "$(id -u)" = 0 ] || { echo "Run it as root (sudo)." >&2; exit 1; }
[ "$(uname -s)" = Linux ] || { echo "Pyo servers run Linux, and this is $(uname -s)." >&2; exit 1; }
case "$(uname -m)" in
  x86_64 | amd64) ;;
  *) echo "Pyo's images are built for x86-64 (amd64), and this machine is $(uname -m)." >&2; exit 1 ;;
esac
command -v curl >/dev/null || { echo "curl is missing. Install it and run this again." >&2; exit 1; }

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

# >>> pyo:repos (the same in connect.sh, join.sh and scripts/join-node.sh)
# Packages come from their vendors' repositories, whose signing keys are
# checked by fingerprint here: Tailscale's and NVIDIA's.
TAILSCALE_KEY=2596A99EAAB33821893C0A79458CA832957F5868
# shellcheck disable=SC2034 # for the GPU setup, which not every script has
NVIDIA_KEY=C95B321B61E88C1809C4F759DDCAE044F796ECB0
os_field() { sed -n "s/^$1=//p" /etc/os-release 2>/dev/null | tr -d '"' | head -n1; }
# signing_key URL FINGERPRINT FILE [armor]: the key with that fingerprint
# from URL into FILE (only that key), or an error.
signing_key() {
  local url=$1 fpr=$2 out=$3 g ok="" pm
  if ! command -v gpg >/dev/null; then
    if command -v apt-get >/dev/null; then
      apt-get update -qq && apt-get install -y -qq gnupg >/dev/null
    elif pm=$(command -v dnf || command -v yum); then
      "$pm" install -y -q gnupg2 >/dev/null
    fi || true
    command -v gpg >/dev/null || { echo "gpg is missing, so the signing key at $url can't be checked. Install gnupg and run this again." >&2; return 1; }
  fi
  g=$(mktemp -d)
  if ! curl -fsSL --proto '=https' --retry 3 --max-time 60 -o "$g/key" "$url"; then
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

# Tailscale: the cluster talks over your tailnet.
tailscale_install
systemctl enable --now tailscaled
if [ -z "$(tailscale ip -4 2>/dev/null)" ]; then
  [ -n "$TS_AUTHKEY" ] || { echo "Not on the tailnet yet: add --tailscale-key tskey-auth-..., or save a key in Pyo Integrations." >&2; exit 1; }
  tailscale up --authkey "$TS_AUTHKEY" --hostname "$NODE_NAME"
fi
TS_IP=$(tailscale ip -4 | head -n1)

# >>> pyo:tailnet-host (the same in connect.sh and join.sh)
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

# The control plane and Pyo's registry door by tailnet name, now that this
# machine is on the tailnet: k3s and containerd keep their IPs.
SERVER_IP=$(tailnet_host "$SERVER_IP")
[ -z "$REGISTRY" ] || REGISTRY=$(tailnet_hostport "$REGISTRY")

# Firewall: cluster traffic over the tailnet, web traffic from anywhere.
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow in on tailscale0 comment 'k3s over Tailscale' >/dev/null
  ufw allow 80/tcp comment 'Pyo ingress' >/dev/null
  ufw allow 443/tcp comment 'Pyo ingress' >/dev/null
fi

# k3s agent, matching the cluster's version; Pyo-built images come from
# the control plane's registry.
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
    echo "Installing the NVIDIA Container Toolkit, so containers can use the GPU..."
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
    # shellcheck disable=SC2034 # connect.sh uses it; here the k3s installer restarts k3s anyway
    GPU_TOOLKIT_NEW=1
  fi
  # The model as a label value: "NVIDIA GeForce RTX 4090" -> NVIDIA-GeForce-RTX-4090.
  model=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -n1 | tr -c 'A-Za-z0-9._-' '-' | cut -c1-63 | sed 's/--*/-/g; s/^[-._]*//; s/[-._]*$//')
  mem=$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits 2>/dev/null | head -n1 | tr -dc 0-9)
  GPU_LABELS="pyo/gpu=nvidia${model:+ pyo/gpu-model=$model}${mem:+ pyo/gpu-memory-mib=$mem}"
  echo "NVIDIA GPU ready for containers: $(nvidia-smi -L | head -n1)"
}
nvidia_gpu

# The cluster's k3s, downloaded and checked before anything changes.
K3S_DIR=$(mktemp -d)
trap 'rm -rf "$K3S_DIR"' EXIT
k3s_fetch "$K3S_DIR" "$K3S_VERSION"

mkdir -p /etc/rancher/k3s
# Images Pyo builds, through its registry door with the cluster's login.
if [ -n "$REGISTRY" ] && [ -n "$REGISTRY_TOKEN" ]; then
  (umask 077 && cat > /etc/rancher/k3s/registries.yaml <<REG
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
  chmod 600 /etc/rancher/k3s/registries.yaml
fi
cat > /etc/rancher/k3s/config.yaml <<CFG
node-ip: "$TS_IP"
flannel-iface: tailscale0
node-name: "$NODE_NAME"
node-label:
  - "pyo/via=tailscale"
CFG
for l in $GPU_LABELS; do echo "  - \"$l\"" >> /etc/rancher/k3s/config.yaml; done
# A build-only server: labelled, and tainted so nothing but the builder
# (which tolerates it) is scheduled here.
if [ -n "$BUILDER" ]; then
  cat >> /etc/rancher/k3s/config.yaml <<CFG
  - "pyo/builder=true"
node-taint:
  - "pyo/builder=true:NoSchedule"
CFG
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
    echo "Note: $f isn't Pyo's, so it's left alone. On this kernel, containers may hang:" >&2
    echo "  https://github.com/containerd/containerd/issues/14485" >&2
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
  echo "Applied Pyo's AppArmor workaround for kernel $kv ($f)."
}
apparmor_workaround

K3S_URL="https://$SERVER_IP:6443" K3S_TOKEN="$K3S_TOKEN" k3s_install "$K3S_DIR" agent
if [ -n "$BUILDER" ]; then
  echo "Joined as $NODE_NAME ($TS_IP), for builds only. The builder starts on it in a minute."
else
  echo "Joined as $NODE_NAME ($TS_IP). It shows up in Pyo in a few seconds."
fi
