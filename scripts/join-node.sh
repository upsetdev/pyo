#!/usr/bin/env bash
# Join this machine to a Pyo k3s cluster over Tailscale, as a worker node.
# Run as root on the new machine:
#
#   SERVER_IP=100.x.y.z K3S_TOKEN=... TS_AUTHKEY=tskey-auth-... \
#   NODE_NAME=worker-1 bash join-node.sh
#
#   SERVER_IP   Tailscale IP of the k3s server (scripts/tailscale-server.sh prints it)
#   K3S_TOKEN   the cluster join token
#   TS_AUTHKEY  Tailscale auth key to join the tailnet (skip if already joined)
#   NODE_NAME   node and Tailscale host name
#   K3S_VERSION the server's k3s version (k3s --version there); Pyo's pinned
#               one (deploy/k3s.env) when unset
#   REGISTRY_TOKEN the cluster's pull token at Pyo's registry door (the
#               --registry-token of Pyo's "Add server" command); without
#               it, the node can't pull images Pyo builds
#
# The node pulls Pyo-built images (127.0.0.1:5005/...) through the server's
# registry door over the tailnet (with REGISTRY_TOKEN), and serves
# HTTP/HTTPS ingress on its own public address (ports 80 and 443).
set -euo pipefail

: "${SERVER_IP:?}" "${K3S_TOKEN:?}" "${NODE_NAME:?}"
export DEBIAN_FRONTEND=noninteractive

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

# ── Tailscale ──────────────────────────────────────────────────────────────
tailscale_install
systemctl enable --now tailscaled
if [ -z "$(tailscale ip -4 2>/dev/null)" ]; then
  : "${TS_AUTHKEY:?this machine is not on the tailnet yet, set TS_AUTHKEY}"
  tailscale up --authkey "$TS_AUTHKEY" --hostname "$NODE_NAME"
fi
TS_IP=$(tailscale ip -4 | head -n1)
echo "Tailscale IP: $TS_IP"

# ── Firewall: cluster traffic over the tailnet, ingress from anywhere ───────
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow in on tailscale0 comment 'k3s over Tailscale' >/dev/null
  ufw allow 80/tcp comment 'Pyo ingress' >/dev/null
  ufw allow 443/tcp comment 'Pyo ingress' >/dev/null
fi

# ── k3s agent ──────────────────────────────────────────────────────────────
K3S_DIR=$(mktemp -d)
trap 'rm -rf "$K3S_DIR"' EXIT
command -v k3s >/dev/null || k3s_fetch "$K3S_DIR" "${K3S_VERSION:-$PYO_K3S_VERSION}"
mkdir -p /etc/rancher/k3s
if [ -n "${REGISTRY_TOKEN:-}" ]; then
  (umask 077 && cat > /etc/rancher/k3s/registries.yaml <<REG
# Pyo-built images are named 127.0.0.1:5005/...; fetch them through the
# server's registry door over the tailnet, with the cluster's login.
mirrors:
  "127.0.0.1:5005":
    endpoint:
      - "http://$SERVER_IP:5005"
configs:
  "$SERVER_IP:5005":
    auth:
      username: pyo-cluster
      password: "$REGISTRY_TOKEN"
REG
  )
else
  echo "No REGISTRY_TOKEN: this node won't pull images Pyo builds." >&2
fi
cat > /etc/rancher/k3s/config.yaml <<CFG
node-ip: "$TS_IP"
flannel-iface: tailscale0
node-name: "$NODE_NAME"
node-label:
  - "pyo/via=tailscale"
CFG
if ! command -v k3s >/dev/null; then
  K3S_URL="https://$SERVER_IP:6443" K3S_TOKEN="$K3S_TOKEN" k3s_install "$K3S_DIR" agent
else
  systemctl restart k3s-agent
fi
echo "Joined as $NODE_NAME ($TS_IP)."
