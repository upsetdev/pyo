#!/usr/bin/env bash
# Makes this machine a Pyo sandbox server: sandboxes run here as microVMs,
# outside any cluster. Pyo's "Add a sandbox server" gives you the command:
#
#   curl -fsSL https://your-pyo/install/sandbox.sh | bash -s -- \
#     --pyo pyo.tail1234.ts.net:5006 --server sbs_abc123 --name sbx-1 \
#     --code 9f2c... --tailscale-key tskey-auth-... --version 0.4.0
#
#   --pyo            Pyo's tailnet door, where the server joins
#   --server         this server's ID in Pyo
#   --name           its name on the tailnet
#   --code           a one-time join code, valid 15 minutes
#   --tailscale-key  joins the tailnet when the machine isn't on it yet
#   --version        the Pyo release whose sandbox files it installs
#   --bundle X       the sandbox files from a file or URL instead of the
#                    release (PYO_SANDBOX_BUNDLE)
#   --edge-domain D  serve sandbox ports at <port>-<sandbox>.D on ports 80
#                    and 443 (PYO_SANDBOX_EDGE_DOMAIN). Needs a wildcard DNS
#                    record *.D pointing at this server.
#   --edge-cert F    the certificate for *.D (PEM, full chain). Default:
#                    /etc/pyo-sandbox/edge/cert.pem
#   --edge-key F     its private key. Default: /etc/pyo-sandbox/edge/key.pem
#
# Run it again with --version to update the daemon. A server that has joined
# needs no code then. Running sandboxes keep running.
#
# Or take it off this machine (sandboxes end, Tailscale stays):
#
#   curl -fsSL https://your-pyo/install/sandbox.sh | bash -s -- --uninstall
#
# It joins your tailnet, installs the daemon (pyo-sandbox), pyo-envd,
# Firecracker, its jailer and the guest kernel, and checks what this server
# can run: Firecracker where it has KVM, gVisor elsewhere. Then it trades
# the code for the daemon's config at Pyo over the tailnet, and starts the
# daemon. The daemon answers only on the tailnet.
#
# This server's own settings go in /etc/pyo-sandbox/local.json: the
# sandboxes' network (their address pool, DNS and blocked ranges) and, with
# --edge-domain, the edge proxy that serves sandbox ports on 80 and 443.
# Edit it and restart pyo-sandbox to change them. A run without
# --edge-domain keeps it as it is. HTTPS starts once the certificate and
# key are in place: put them there and run this again with --edge-domain.
# With ufw active, this opens 80 and 443 for the edge. Any other firewall
# in front of the server needs them open too.
#
# Everything is in functions and only the last line runs anything, so a
# download that stops halfway does nothing.
set -euo pipefail

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

# >>> pyo:tailnet-lock (the same in connect.sh, join.sh and deploy/sandbox.sh)
# With Tailnet Lock on, a new machine waits until a signing device signs
# its key: the command to run there, then the wait (up to an hour).
tailnet_lock_wait() {
  local js key i
  js=$(tailscale lock status --json 2>/dev/null) || return 0
  grep -Eq '"Enabled": *true' <<<"$js" || return 0
  grep -Eq '"NodeKeySigned": *true' <<<"$js" && return 0
  key=$(sed -n 's/^ *"NodeKey": *"\([^"]*\)".*/\1/p' <<<"$js" | head -n1)
  echo "Your tailnet has Tailnet Lock on. On a signing device, run:"
  echo "  tailscale lock sign $key"
  echo "Waiting for the signature..."
  for i in $(seq 720); do
    tailscale lock status --json 2>/dev/null | grep -Eq '"NodeKeySigned": *true' && { echo "Signed."; return 0; }
    sleep 5
  done
  echo "This machine isn't signed yet. Sign it and run this again." >&2
  exit 1
}
# <<< pyo:tailnet-lock

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

defaults() {
  PYO="" SERVER="" NAME="" CODE="" VERSION=${PYO_VERSION:-} BUNDLE=${PYO_SANDBOX_BUNDLE:-}
  TS_AUTHKEY=${TS_AUTHKEY:-} UNINSTALL=""
  RELEASES=${PYO_RELEASES:-https://github.com/upsetdev/pyo/releases/download}
  LIB=/usr/local/lib/pyo-sandbox BIN=/usr/local/bin/pyo-sandbox
  ETC=/etc/pyo-sandbox DATA=/var/lib/pyo-sandbox
  UNIT=/etc/systemd/system/pyo-sandbox.service
  PORT=7070 WORK="" TS_IP="" JAIL_UID="" JAIL_GID=""
  EDGE_DOMAIN=${PYO_SANDBOX_EDGE_DOMAIN:-} EDGE_CERT="" EDGE_KEY=""
  EDGE_HTTP=80 EDGE_HTTPS=443
}

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
fail() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --pyo) PYO="${2:-}"; shift 2 ;;
      --server) SERVER="${2:-}"; shift 2 ;;
      --name) NAME="${2:-}"; shift 2 ;;
      --code) CODE="${2:-}"; shift 2 ;;
      --tailscale-key) TS_AUTHKEY="${2:-}"; shift 2 ;;
      --version) VERSION="${2:-}"; shift 2 ;;
      --bundle) BUNDLE="${2:-}"; shift 2 ;;
      --edge-domain) EDGE_DOMAIN="${2:-}"; shift 2 ;;
      --edge-cert) EDGE_CERT="${2:-}"; shift 2 ;;
      --edge-key) EDGE_KEY="${2:-}"; shift 2 ;;
      --uninstall) UNINSTALL=1; shift ;;
      -h | --help)
        echo "Usage: sandbox.sh --pyo HOST:PORT --server ID --name NAME --code CODE --version X [--tailscale-key KEY] [--bundle FILE|URL]"
        echo "                  [--edge-domain DOMAIN [--edge-cert FILE] [--edge-key FILE]]"
        echo "       sandbox.sh --uninstall"
        exit 0
        ;;
      *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
  done
  [ -n "$UNINSTALL" ] && return 0
  EDGE_DOMAIN=${EDGE_DOMAIN%.} EDGE_DOMAIN=${EDGE_DOMAIN,,}
  [ -z "$EDGE_DOMAIN" ] || [[ $EDGE_DOMAIN =~ ^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z][a-z0-9-]{0,62}$ ]] || fail "\"$EDGE_DOMAIN\" isn't a domain, like sbx.example.com."
  [ -n "$EDGE_DOMAIN" ] || [ -z "$EDGE_CERT$EDGE_KEY" ] || fail "--edge-cert and --edge-key go with --edge-domain."
  local f
  for f in "$EDGE_CERT" "$EDGE_KEY"; do
    [ -z "$f" ] || [[ $f == /* && $f != *[\"\\]* ]] || fail "\"$f\" isn't a full path, like /etc/ssl/sbx.pem."
  done
  VERSION=${VERSION#v}
  [ -n "$VERSION" ] || [ -n "$BUNDLE" ] || fail "Give the Pyo release with --version, like 0.4.0. Copy the full command from Pyo."
  [ -z "$VERSION" ] || [[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || fail "\"$VERSION\" isn't a release version, like 0.4.0."
  if [ -z "$CODE" ] && joined; then
    return 0
  fi
  [ -n "$PYO" ] && [ -n "$SERVER" ] && [ -n "$NAME" ] && [ -n "$CODE" ] || fail "Copy the full command from Pyo: it has --pyo, --server, --name and --code."
  [[ $SERVER =~ ^[a-z0-9_]{1,64}$ ]] || fail "\"$SERVER\" isn't a sandbox server ID."
  [[ $CODE =~ ^[0-9a-f]{16,256}$ ]] || fail "The join code is wrong. Copy the full command from Pyo."
  [[ $NAME =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] || fail "\"$NAME\" isn't a server name: lowercase letters, digits and dashes."
}

# Joined before: the daemon's config is there.
joined() { [ -s "${ETC:-/etc/pyo-sandbox}/config.json" ]; }

# Root, or again with sudo. Piped from curl there's no file to run again, so
# the script travels as its functions plus the settings it sets outside them
# (SETTINGS, which scripts/sandbox.test.sh checks against the script). sudo
# clears the environment, so every PYO_* variable goes along too.
SETTINGS="TAILSCALE_KEY NVIDIA_KEY"
become_root() {
  [ "$(id -u)" = 0 ] && return 0
  command -v sudo >/dev/null || fail "Run this as root (there's no sudo)."
  say "Pyo needs root. Asking sudo"
  local keep=() v settings
  for v in $(compgen -e -X '!PYO_*'); do keep+=("$v=${!v}"); done
  # shellcheck disable=SC2086 # a list of names
  settings=$(declare -p SETTINGS $SETTINGS)
  # shellcheck disable=SC2016 # expanded by the new shell
  exec sudo env "${keep[@]}" TS_AUTHKEY="$TS_AUTHKEY" \
    bash -c "$settings; $(declare -f); main \"\$@\"" sandbox.sh "$@"
}

check_system() {
  [ "$(uname -s)" = Linux ] || fail "Sandbox servers run Linux, and this is $(uname -s)."
  case "$(uname -m)" in
    x86_64 | amd64) ARCH=x86_64 ;;
    aarch64 | arm64) ARCH=aarch64 ;;
    *) fail "Sandbox servers are x86-64 or arm64, and this is $(uname -m)." ;;
  esac
  local tool
  for tool in curl tar sha256sum systemctl mkfs.ext4; do
    command -v "$tool" >/dev/null || fail "$tool is missing. Install it and run this again."
  done
  if command -v k3s >/dev/null || [ -d /var/lib/rancher/k3s ]; then
    fail "This machine runs k3s, so it's a cluster's server. Sandbox servers run nothing else: use another machine."
  fi
}

# Where the release keeps the sandbox server's files.
bundle_url() { printf '%s/v%s/pyo-sandbox-linux-%s.tar.gz\n' "$RELEASES" "$VERSION" "$ARCH"; }

# The sandbox server's files, checked, before anything changes:
# pyo-sandbox, pyo-envd, firecracker, jailer and vmlinux.
fetch_bundle() {
  local tgz=$WORK/bundle.tar.gz src=$BUNDLE sum
  if [ -n "$src" ] && [ -f "$src" ]; then
    cp "$src" "$tgz"
  else
    # The release's own: .github/workflows/sandbox.yml packs it with
    # crates/pyo-sandbox/scripts/bundle.sh, its .sha256 beside it.
    [ -n "$src" ] || src=$(bundle_url)
    say "Downloading $src"
    curl -fsSL --proto '=https,http' --retry 3 -o "$tgz" "$src" || fail "Couldn't download $src."
    if [ -z "$BUNDLE" ]; then
      sum=$(curl -fsSL --retry 3 "$src.sha256" | awk '{print $1}') || fail "Couldn't download $src.sha256."
      echo "$sum  $tgz" | sha256sum -c --quiet - || fail "$src doesn't match its checksum."
    fi
  fi
  mkdir -p "$WORK/bundle"
  tar -xzf "$tgz" -C "$WORK/bundle" || fail "The sandbox files aren't a .tar.gz."
  local f
  for f in pyo-sandbox pyo-envd firecracker jailer vmlinux; do
    [ -s "$WORK/bundle/$f" ] || fail "The sandbox files have no $f."
  done
}

join_tailnet() {
  tailscale_install
  systemctl enable --now tailscaled >/dev/null
  if [ -z "$(tailscale ip -4 2>/dev/null)" ]; then
    [ -n "$TS_AUTHKEY" ] || fail "Not on the tailnet yet: copy the full command from Pyo."
    say "Joining your tailnet as ${NAME:-this server}"
    tailscale up --authkey "$TS_AUTHKEY" ${NAME:+--hostname "$NAME"} --timeout 120s || true
  fi
  tailnet_lock_wait
  TS_IP=$(tailscale ip -4 2>/dev/null | head -n1 || true)
  [ -n "$TS_IP" ] || fail "This machine didn't join the tailnet (see above)."
  [ -z "$PYO" ] || PYO=$(tailnet_hostport "$PYO")
}

# The user the jailer drops each VM to.
make_user() {
  id -u pyo-sandbox >/dev/null 2>&1 ||
    useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin pyo-sandbox
  JAIL_UID=$(id -u pyo-sandbox)
  JAIL_GID=$(id -g pyo-sandbox)
}

# Into place by rename, so a running daemon keeps its binary until it
# restarts.
install_files() {
  local f
  install -d -m 755 "$LIB" "$ETC" "$DATA"
  for f in pyo-envd firecracker jailer; do
    install -m 755 "$WORK/bundle/$f" "$LIB/.$f.new" && mv -f "$LIB/.$f.new" "$LIB/$f"
  done
  install -m 644 "$WORK/bundle/vmlinux" "$LIB/.vmlinux.new" && mv -f "$LIB/.vmlinux.new" "$LIB/vmlinux"
  install -m 755 "$WORK/bundle/pyo-sandbox" "$BIN.new" && mv -f "$BIN.new" "$BIN"
}

# A config for the detection alone: no keys, nothing listens.
detect_config() {
  cat <<CFG
{"serverId": "detect", "listen": "127.0.0.1:$PORT", "root": "$DATA",
 "firecracker": "$LIB/firecracker", "jailer": "$LIB/jailer", "kernel": "$LIB/vmlinux",
 "envd": "$LIB/pyo-envd", "keys": [], "jailUid": $JAIL_UID, "jailGid": $JAIL_GID}
CFG
}

# A field of the detection's JSON report.
report_field() { sed -n "s/^ *\"$1\": *\"\{0,1\}\([^\",]*\)\"\{0,1\},\{0,1\}$/\1/p" "$2" | head -n1; }

# What this server can run. Stops here, before the code is used, when it
# can run neither Firecracker nor gVisor.
detect() {
  say "Checking what this server can run"
  detect_config > "$WORK/detect.json"
  "$BIN" detect --config "$WORK/detect.json" > "$WORK/capability.json" || fail "The detection failed (see above)."
  local runtime kvm boot reason
  runtime=$(report_field runtime "$WORK/capability.json")
  kvm=$(report_field kvm "$WORK/capability.json")
  boot=$(report_field bootMs "$WORK/capability.json")
  case "$runtime:$kvm" in
    firecracker:native) echo "Firecracker on KVM. A test VM booted in ${boot:-?} ms." ;;
    firecracker:*) echo "Firecracker on nested KVM. A test VM booted in ${boot:-?} ms, so sandboxes start slower." ;;
    gvisor:*) echo "gVisor, because this server has no usable KVM." ;;
    *)
      reason=$(report_field reason "$WORK/capability.json")
      fail "This server can't run sandboxes. ${reason:-It has neither KVM nor gVisor.}"
      ;;
  esac
}

# The code for the daemon's config, at Pyo's tailnet door. Tried again
# while Pyo can't be reached (with Tailnet Lock on, until the key is
# signed). The first answer burns the code.
redeem() {
  local out status body deadline=$((SECONDS + 600))
  say "Joining Pyo as $NAME"
  {
    printf '{"code": "%s", "address": "%s", "jailUid": %s, "jailGid": %s, "capability": ' "$CODE" "$TS_IP" "$JAIL_UID" "$JAIL_GID"
    cat "$WORK/capability.json"
    printf '}\n'
  } > "$WORK/ask.json"
  while :; do
    out=$(curl -sS -m 30 -w '\n%{http_code}' -X POST -H 'Content-Type: application/json' \
      --data-binary "@$WORK/ask.json" "http://$PYO/sandbox/join?server=$SERVER" 2>&1) || true
    status=${out##*$'\n'} body=${out%$'\n'*}
    case "$status" in
      200) break ;;
      000 | 429 | 5??) ;;
      *) fail "Pyo refused: $body" ;;
    esac
    [ "$SECONDS" -lt "$deadline" ] || fail "Pyo didn't answer over the tailnet in 10 minutes ($body)."
    sleep 5
  done
  rm -f "$WORK/ask.json"
  case "$body" in
    *"\"serverId\":\"$SERVER\""* | *"\"serverId\": \"$SERVER\""*) ;;
    *) fail "Pyo's answer isn't this server's config." ;;
  esac
  (umask 077 && printf '%s\n' "$body" > "$ETC/config.json.new")
  mv -f "$ETC/config.json.new" "$ETC/config.json"
}

# This server's own settings for the daemon: the sandboxes' network and,
# with an edge domain, the edge proxy. HTTPS only once the certificate and
# key are there, so the daemon starts without them.
local_config() {
  local cert=${EDGE_CERT:-$ETC/edge/cert.pem} key=${EDGE_KEY:-$ETC/edge/key.pem} https=""
  if [ -s "$cert" ] && [ -s "$key" ]; then
    https="\"https\": \"0.0.0.0:$EDGE_HTTPS\", "
  fi
  printf '{\n  "network": {"pool": "198.18.0.0/16", "dns": ["1.1.1.1", "8.8.8.8"], "blocked": []}'
  if [ -n "$EDGE_DOMAIN" ]; then
    printf ',\n  "edge": {"domain": "%s", %s"http": "0.0.0.0:%s", "cert": "%s", "key": "%s"}' \
      "$EDGE_DOMAIN" "$https" "$EDGE_HTTP" "$cert" "$key"
  fi
  printf '\n}\n'
}

# Written on the first run and whenever --edge-domain is given. Otherwise
# kept, with whatever was changed in it by hand.
write_local_config() {
  local f=$ETC/local.json cert=${EDGE_CERT:-$ETC/edge/cert.pem} key=${EDGE_KEY:-$ETC/edge/key.pem}
  if [ -s "$f" ] && [ -z "$EDGE_DOMAIN" ]; then
    return 0
  fi
  local_config > "$f.new" && chmod 644 "$f.new" && mv -f "$f.new" "$f"
  [ -n "$EDGE_DOMAIN" ] || return 0
  if [ -s "$cert" ] && [ -s "$key" ]; then
    echo "Sandbox ports are served at https://<port>-<sandbox>.$EDGE_DOMAIN."
  else
    echo "Sandbox ports are served at http://<port>-<sandbox>.$EDGE_DOMAIN for now."
    echo "For HTTPS, put a certificate for *.$EDGE_DOMAIN at $cert and its key at $key, then run this again with --edge-domain $EDGE_DOMAIN."
  fi
}

# Whether the daemon's settings turn the edge proxy on.
edge_on() { grep -q '"edge"' "$ETC/local.json" 2>/dev/null; }

# The daemon's unit. KillMode=process: stopping or updating the daemon stops
# only the daemon, and the sandboxes (VMs it started) keep running. The new
# daemon adopts them.
write_unit() {
  cat > "$UNIT.new" <<UNIT
[Unit]
Description=Pyo Sandbox daemon
Documentation=https://pyo.dev/docs
After=network-online.target tailscaled.service
Wants=network-online.target tailscaled.service

[Service]
ExecStart=$BIN serve --config $ETC/config.json
Restart=always
RestartSec=2
KillMode=process
LimitNOFILE=1048576
TasksMax=infinity
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
UNIT
  mv -f "$UNIT.new" "$UNIT"
}

# The daemon over the tailnet only, and the edge proxy (80 and 443) to
# everyone when it's on. Only ufw is managed here.
firewall() {
  if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
    ufw allow in on tailscale0 to any port "$PORT" proto tcp comment 'Pyo sandbox daemon over Tailscale' >/dev/null
    if edge_on; then
      ufw allow "$EDGE_HTTP/tcp" comment 'Pyo sandbox edge' >/dev/null
      ufw allow "$EDGE_HTTPS/tcp" comment 'Pyo sandbox edge' >/dev/null
    fi
  elif edge_on; then
    echo "If a firewall is in front of this server, open TCP $EDGE_HTTP and $EDGE_HTTPS to it for sandbox ports."
  fi
}

start_daemon() {
  local i
  systemctl daemon-reload
  systemctl enable pyo-sandbox >/dev/null 2>&1
  systemctl restart pyo-sandbox
  for i in $(seq 60); do
    curl -fsS -m 2 "http://$TS_IP:$PORT/v1/health" >/dev/null 2>&1 && return 0
    sleep 1
  done
  journalctl -u pyo-sandbox -n 20 --no-pager >&2 || true
  fail "The sandbox daemon didn't start (see above)."
}

uninstall() {
  say "Removing the Pyo sandbox daemon"
  systemctl disable --now pyo-sandbox >/dev/null 2>&1 || true
  # Its sandboxes: the VMs it started outlive it by design.
  local p
  for p in $(pgrep -x firecracker || true); do
    case "$(readlink "/proc/$p/cwd" 2>/dev/null)" in "$DATA"/*) kill -9 "$p" 2>/dev/null || true ;; esac
  done
  rm -f "$UNIT" "$BIN"
  systemctl daemon-reload || true
  rm -rf "$LIB" "$ETC" "$DATA"
  if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
    ufw delete allow in on tailscale0 to any port "$PORT" proto tcp >/dev/null 2>&1 || true
    # Only the edge rules this script added: they carry its comment.
    if ufw status | grep -q "Pyo sandbox edge"; then
      ufw delete allow "$EDGE_HTTP/tcp" >/dev/null 2>&1 || true
      ufw delete allow "$EDGE_HTTPS/tcp" >/dev/null 2>&1 || true
    fi
  fi
  say "Done. This machine is still on your tailnet. You can remove it in the Tailscale admin console."
}

cleanup() { [ -z "${WORK:-}" ] || rm -rf "$WORK"; }

main() {
  # Again here: after sudo, only the functions and SETTINGS came along.
  set -euo pipefail
  defaults
  parse_args "$@"
  become_root "$@"
  if [ -n "$UNINSTALL" ]; then
    uninstall
    return 0
  fi
  check_system
  WORK=$(mktemp -d)
  trap cleanup EXIT
  fetch_bundle
  join_tailnet
  make_user
  install_files
  detect
  if [ -n "$CODE" ]; then
    redeem
  else
    say "Joined before: updating the daemon"
  fi
  write_local_config
  write_unit
  firewall
  start_daemon
  if [ -n "$CODE" ]; then
    say "$NAME ($TS_IP) is a sandbox server now. It shows up in Pyo in a few seconds."
  else
    say "The sandbox daemon is updated. Its sandboxes kept running."
  fi
}

main "$@"
