#!/usr/bin/env bash
# deploy/sandbox.sh's functions, without a machine to set up: its functions
# are sourced (all but the last line, which runs it), and the commands that
# would change the machine are stubbed. Run: bash scripts/sandbox.test.sh
set -euo pipefail
cd "$(dirname "$0")/.."
export LC_ALL=C

bash -n deploy/sandbox.sh

failed=0
check() { # $1: what, $2: output, $3: grep -E pattern, $4: "has" or "lacks"
  if grep -qE -- "$3" <<<"$2"; then
    [ "$4" = has ] && return 0
  else
    [ "$4" = lacks ] && return 0
  fi
  printf 'FAIL: %s (%s %s)\n%s\n' "$1" "$4" "$3" "$2" >&2
  failed=1
}

# What it sets outside its functions goes along when it runs again with
# sudo (its SETTINGS). Without it, a sudo user's run fails at once.
missing=$(bash -c '
  vars() { compgen -v | sort; }
  a=$(vars)
  source <(sed "\$d" deploy/sandbox.sh)
  comm -13 <(echo "$a") <(vars) | grep -vxE "a|PIPESTATUS|SETTINGS|${SETTINGS// /|}" || true
')
check "every setting outside the functions is in SETTINGS" "$missing" "." lacks

# shellcheck source=/dev/null
source <(sed '$d' deploy/sandbox.sh)
set +e
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
code=$(printf '%064d' 7)
args=(--pyo pyo.tail1234.ts.net:5006 --server sbs_abc123 --name sbx-1 --code "$code" --tailscale-key tskey-auth-x --version 0.4.0)

# ── Options ──
defaults
ETC=$tmp/etc
out=$( (parse_args "${args[@]}" && echo "parsed $SERVER $NAME $VERSION") 2>&1)
check "the full command parses" "$out" "^parsed sbs_abc123 sbx-1 0.4.0$" has
out=$( (defaults; ETC=$tmp/etc; parse_args --pyo x:1 --server sbs_a --name n --version 0.4.0) 2>&1)
check "a first join needs a code" "$out" "Copy the full command from Pyo" has
out=$( (defaults; ETC=$tmp/etc; parse_args --pyo x:1 --server sbs_a --name n --code 'abc;rm' --version 0.4.0) 2>&1)
check "a code is hex only" "$out" "join code is wrong" has
out=$( (defaults; ETC=$tmp/etc; parse_args --pyo x:1 --server sbs_a --name Bad_Name --code "$code" --version 0.4.0) 2>&1)
check "names are DNS labels" "$out" "isn't a server name" has
out=$( (defaults; ETC=$tmp/etc; parse_args --server sbs_a --code "$code") 2>&1)
check "a release or a bundle is needed" "$out" "--version" has
(defaults; parse_args --not-an-option) >/dev/null 2>&1
check "an unknown option exits 2" "$?" "^2$" has
out=$( (defaults; parse_args --uninstall && echo "uninstall=$UNINSTALL") 2>&1)
check "--uninstall needs nothing else" "$out" "^uninstall=1$" has
mkdir -p "$tmp/etc" && echo '{}' >"$tmp/etc/config.json"
out=$( (defaults; ETC=$tmp/etc; parse_args --version 0.4.1 && echo updating) 2>&1)
check "a joined server updates without a code" "$out" "^updating$" has
rm -f "$tmp/etc/config.json"

# ── The release's files ──
defaults
ARCH=x86_64 VERSION=0.4.0
check "the release URL" "$(bundle_url)" "^https://github.com/upsetdev/pyo/releases/download/v0.4.0/pyo-sandbox-linux-x86_64.tar.gz$" has
mkdir -p "$tmp/b"
for f in pyo-sandbox pyo-envd firecracker jailer vmlinux; do echo "$f" >"$tmp/b/$f"; done
tar -czf "$tmp/bundle.tar.gz" -C "$tmp/b" .
WORK=$tmp/work1 BUNDLE=$tmp/bundle.tar.gz
mkdir -p "$WORK"
out=$( (fetch_bundle && ls "$WORK/bundle") 2>&1)
check "a local bundle is used as it is" "$out" "vmlinux" has
rm "$tmp/b/jailer" && tar -czf "$tmp/short.tar.gz" -C "$tmp/b" .
WORK=$tmp/work2 BUNDLE=$tmp/short.tar.gz
mkdir -p "$WORK"
out=$( (fetch_bundle) 2>&1)
check "a bundle without the jailer is refused" "$out" "have no jailer" has
# A download shows progress, and its failure says what to do, without
# curl's own words.
curl() { echo "curl: (6) Could not resolve host" >&2; return 6; }
WORK=$tmp/work2b BUNDLE=""
mkdir -p "$WORK"
out=$( (fetch_bundle) 2>&1)
check "the download shows progress" "$out" "^Downloading the sandbox files… 0:00$" has
check "a failed download says to run it again" "$out" "Couldn't download the sandbox files from https://github.com/.*run the same command again\.$" has
curl() {
  local a o=""
  for a in "$@"; do [ "$o" = 1 ] && { echo bundle >"$a"; o=""; }; [ "$a" = -o ] && o=1; done
  case "$*" in *.sha256*) echo "0000  pyo-sandbox-linux-x86_64.tar.gz" ;; esac
  return 0
}
WORK=$tmp/work2c
mkdir -p "$WORK"
out=$( (fetch_bundle) 2>&1)
check "a bad checksum says to run it again" "$out" "don't match their checksum. Run the same command again\.$" has
check "and shows no sha256sum output" "$out" "FAILED|WARNING" lacks
unset -f curl

# ── Detection ──
defaults
WORK=$tmp/work3 JAIL_UID=998 JAIL_GID=997 BIN=$tmp/pyo-sandbox
mkdir -p "$WORK"
fake_detect() { # the daemon's `detect`, printing $1
  printf '#!/bin/sh\ncat <<JSON\n%s\nJSON\n' "$1" >"$BIN"
  chmod +x "$BIN"
}
fake_detect '{
  "runtime": "firecracker",
  "kvm": "native",
  "bootMs": 73,
  "arch": "x86_64",
  "cpus": 22,
  "checks": [
    {
      "name": "kvm",
      "ok": true,
      "detail": "runtime: none here"
    }
  ]
}'
out=$( (detect) 2>&1)
check "Firecracker on KVM is reported" "$out" "Firecracker on KVM. A test VM booted in 73 ms." has
check "the detection shows progress" "$out" "^Checking what this server can run… 0:00$" has
check "and no raw JSON" "$out" '"runtime"|[{}]' lacks
check "the detection gets this server's jail user" "$(detect_config)" '"jailUid": 998, "jailGid": 997' has
fake_detect '{
  "runtime": "none",
  "kvm": "none",
  "arch": "x86_64",
  "reason": "No usable KVM and gVisor is not installed."
}'
out=$( (detect && echo "went on") 2>&1)
check "a server that can run nothing stops before its code is used" "$out" "can't run sandboxes. No usable KVM and gVisor is not installed." has
check "and goes no further" "$out" "went on" lacks
fake_detect '{
  "runtime": "gvisor",
  "kvm": "none",
  "arch": "x86_64"
}'
check "gVisor is reported" "$( (detect) 2>&1)" "gVisor, because this server has no usable KVM" has
printf '#!/bin/sh\necho "kvm: permission denied" >&2\nexit 1\n' >"$BIN"
out=$( (detect && echo "went on") 2>&1)
check "a failed detection shows why" "$out" "kvm: permission denied" has
check "and says to run it again" "$out" "Couldn't check what this server can run. Run the same command again\.$" has
check "and stops" "$out" "went on" lacks

# ── Joining the tailnet ──
defaults
NAME=sbx-1 TS_AUTHKEY=tskey-auth-x PYO=100.64.0.1:5006
systemctl() { :; }
tailnet_lock_wait() { :; }
tailscale() {
  case "$1" in
    ip) [ ! -f "$tmp/ts-up" ] || echo 100.64.0.9 ;;
    up) echo "Some peers are advertising routes" >&2; [ -z "${ts_fails:-}" ] && touch "$tmp/ts-up" ;;
  esac
  return 0
}
rm -f "$tmp/ts-up"
out=$( (join_tailnet && echo "ip=$TS_IP pyo=$PYO") 2>&1)
check "joining the tailnet shows progress" "$out" "^Joining your tailnet… 0:00$" has
check "and hides Tailscale's own output" "$out" "advertising" lacks
check "and gets this server's address" "$out" "^ip=100.64.0.9 pyo=100.64.0.1:5006$" has
rm -f "$tmp/ts-up"
out=$( (ts_fails=1 join_tailnet) 2>&1)
check "a failed join says to run a fresh command" "$out" "didn't join your tailnet. Copy a fresh command from Pyo and run it again\.$" has
out=$( (TS_AUTHKEY="" join_tailnet) 2>&1)
check "no key, no join" "$out" "Not on your tailnet yet" has
unset -f systemctl tailscale tailnet_lock_wait

# ── Joining Pyo ──
sleep() { :; }
curl() {
  local a
  for a in "$@"; do case "$a" in @*) cp "${a#@}" "$tmp/asked.json" ;; http*) echo "$a" >"$tmp/url" ;; esac; done
  printf "%s\n%s" "$fake_body" "$fake_code"
}
defaults
ETC=$tmp/etc2 WORK=$tmp/work4 PYO=100.64.0.1:5006 SERVER=sbs_abc123 NAME=sbx-1 CODE=$code TS_IP=100.64.0.9 JAIL_UID=998 JAIL_GID=997
mkdir -p "$ETC" "$WORK"
printf '{\n  "runtime": "firecracker"\n}\n' >"$WORK/capability.json"
fake_body='{"serverId":"sbs_abc123","listen":"100.64.0.9:7070","keys":[]}' fake_code=200
out=$( (redeem) 2>&1)
check "the code is traded for the config" "$(cat "$ETC/config.json" 2>/dev/null)" '"listen":"100.64.0.9:7070"' has
check "at Pyo's tailnet door, for this server" "$(cat "$tmp/url")" "^http://100.64.0.1:5006/sandbox/join\?server=sbs_abc123$" has
check "with the code, address, user and report" "$(cat "$tmp/asked.json")" "\"code\": \"$code\", \"address\": \"100.64.0.9\", \"jailUid\": 998, \"jailGid\": 997, \"capability\": \{" has
check "the config is root's only" "$(stat -c %a "$ETC/config.json")" "^600$" has
check "the ask is valid JSON" "$(python3 -c 'import json,sys; json.load(open(sys.argv[1])); print("json")' "$tmp/asked.json" 2>&1)" "^json$" has
rm -f "$ETC/config.json"
fake_body='This code expired. Make a new one in Pyo.' fake_code=403
out=$( (redeem) 2>&1)
check "a refusal is shown as Pyo says it" "$out" "Pyo refused this server. This code expired. Make a new one in Pyo." has
fake_body='{"error":"This join code was used already."}' fake_code=409
out=$( (redeem) 2>&1)
check "a JSON refusal shows only its words" "$out" "Pyo refused this server. This join code was used already.$" has
check "and writes no config" "$(ls "$ETC")" "config.json" lacks
fake_body='{"serverId":"sbs_other"}' fake_code=200
out=$( (redeem) 2>&1)
check "another server's config is refused" "$out" "isn't this server's config" has
fake_body='{"serverId":"sbs_abc123"}' fake_code=200
out=$( (redeem) 2>&1)
check "joining Pyo shows progress" "$out" "^Joining Pyo as sbx-1… 0:00$" has
# Pyo out of reach: the wait ends after 10 minutes, without curl's words.
sleep() { SECONDS=$((SECONDS + ${1%.*})); }
fake_body='curl: (7) Failed to connect' fake_code=000
out=$( (redeem) 2>&1)
check "an unreachable Pyo says what to do" "$out" "didn't answer over your tailnet in 10 minutes. Check that Pyo is running, then run the same command again\.$" has
check "and not curl's error" "$out" "curl:" lacks
check "the wait shows every 20 seconds in a log" "$out" "^Joining Pyo as sbx-1… 0:20$" has
unset -f curl sleep

# ── Starting the daemon ──
defaults
TS_IP=100.64.0.9
systemctl() { :; }
journalctl() { echo "pyo-sandbox: bad config"; }
sleep() { SECONDS=$((SECONDS + ${1%.*})); }
curl() { [ -f "$tmp/up" ]; }
touch "$tmp/up"
out=$( (start_daemon && echo started) 2>&1)
check "a daemon that's up at once: nothing but done" "$out" "^started$" has
check "and no log" "$out" "bad config" lacks
rm -f "$tmp/up"
out=$( (start_daemon && echo started) 2>&1)
check "a daemon that won't start shows progress" "$out" "^Starting the sandbox daemon… 0:00$" has
check "and its log" "$out" "pyo-sandbox: bad config" has
check "and says to run it again" "$out" "didn't start. Its last log lines are above. Run the same command again\.$" has
check "and doesn't go on" "$out" "started$" lacks
unset -f systemctl journalctl sleep curl

# ── The daemon's unit ──
defaults
UNIT=$tmp/pyo-sandbox.service
write_unit
unit=$(cat "$UNIT")
check "sandboxes outlive daemon restarts" "$unit" "^KillMode=process$" has
check "the unit runs the daemon with its config" "$unit" "^ExecStart=/usr/local/bin/pyo-sandbox serve --config /etc/pyo-sandbox/config.json$" has
check "it restarts" "$unit" "^Restart=always$" has

# ── This server's settings: the network and the edge ──
defaults
ETC=$tmp/etc3
mkdir -p "$ETC"
out=$( (parse_args "${args[@]}" --edge-domain SBX.Example.com. && echo "edge=$EDGE_DOMAIN") 2>&1)
check "an edge domain is taken in lowercase" "$out" "^edge=sbx.example.com$" has
out=$( (defaults; ETC=$tmp/etc3; parse_args "${args[@]}" --edge-domain 'sbx example') 2>&1)
check "an edge domain is a domain" "$out" "isn't a domain" has
out=$( (defaults; ETC=$tmp/etc3; parse_args "${args[@]}" --edge-cert /etc/x.pem) 2>&1)
check "a certificate goes with a domain" "$out" "go with --edge-domain" has
out=$( (defaults; ETC=$tmp/etc3; parse_args "${args[@]}" --edge-domain sbx.example.com --edge-key key.pem) 2>&1)
check "certificate paths are full paths" "$out" "isn't a full path" has
json_ok() { python3 -c 'import json,sys; json.load(open(sys.argv[1])); print("json")' "$1" 2>&1; }

defaults
ETC=$tmp/etc3
out=$(write_local_config 2>&1)
local_json=$(cat "$ETC/local.json")
check "the first run writes the network settings" "$local_json" '"network": \{"pool": "198.18.0.0/16", "dns": \["1.1.1.1", "8.8.8.8"\]' has
check "and no edge without a domain" "$local_json" '"edge"' lacks
check "the settings are valid JSON" "$(json_ok "$ETC/local.json")" "^json$" has
EDGE_DOMAIN=sbx.example.com
out=$(write_local_config 2>&1)
local_json=$(cat "$ETC/local.json")
check "the edge listens on 443 and 80" "$local_json" "\"edge\": \{\"domain\": \"sbx.example.com\", \"https\": \"0.0.0.0:443\", \"http\": \"0.0.0.0:80\", \"cert\": \"$ETC/edge/cert.pem\", \"key\": \"$ETC/edge/key.pem\"\}" has
check "it serves HTTP until there's a certificate" "$out" "http://<port>-<sandbox>.sbx.example.com until" has
check "it says where the certificate goes" "$out" "put a certificate for \*.sbx.example.com at $ETC/edge/cert.pem" has
check "the edge settings are valid JSON" "$(json_ok "$ETC/local.json")" "^json$" has
mkdir -p "$ETC/edge" && echo cert >"$ETC/edge/cert.pem" && echo key >"$ETC/edge/key.pem"
out=$(write_local_config 2>&1)
check "with the certificate, HTTPS on 443" "$(cat "$ETC/local.json")" '"https": "0.0.0.0:443", "http": "0.0.0.0:80"' has
check "and it says so" "$out" "served at https://<port>-<sandbox>.sbx.example.com" has
check "still valid JSON" "$(json_ok "$ETC/local.json")" "^json$" has
EDGE_DOMAIN=""
echo '{"edge": {"domain": "edited.example.com"}}' >"$ETC/local.json"
write_local_config
check "a run without a domain keeps the settings" "$(cat "$ETC/local.json")" "edited.example.com" has

# ── The firewall ──
ufw() {
  echo "ufw $*" >>"$tmp/ufw"
  [ "$1" = status ] && echo "Status: ${ufw_state:-active}"
  return 0
}
ufw_state=active
rm -f "$tmp/ufw"
firewall
check "ufw: the daemon on the tailnet" "$(cat "$tmp/ufw")" "ufw allow in on tailscale0 to any port 7070 proto tcp" has
check "ufw: the edge's ports" "$(cat "$tmp/ufw")" "ufw allow 80/tcp comment Pyo sandbox edge" has
check "ufw: and 443" "$(cat "$tmp/ufw")" "ufw allow 443/tcp" has
echo '{"network": {}}' >"$ETC/local.json"
rm -f "$tmp/ufw"
firewall
check "ufw: no edge, no 80" "$(cat "$tmp/ufw")" "80/tcp" lacks
echo '{"edge": {"domain": "sbx.example.com"}}' >"$ETC/local.json"
ufw_state=inactive
rm -f "$tmp/ufw"
out=$(firewall 2>&1)
check "without ufw, it says which ports to open" "$out" "open TCP 80 and 443" has
check "and changes no firewall" "$(cat "$tmp/ufw")" "allow" lacks
unset -f ufw

# ── sudo: the root shell gets the functions, SETTINGS and PYO_* ──
bin=$tmp/fakebin
mkdir -p "$bin"
cat >"$bin/id" <<'SH'
#!/bin/sh
[ "$1" = -u ] && [ -n "${FAKE_ROOT:-}" ] && { echo 0; exit 0; }
exec /usr/bin/id "$@"
SH
cat >"$bin/sudo" <<'SH'
#!/bin/sh
exec env FAKE_ROOT=1 "$@"
SH
chmod +x "$bin/id" "$bin/sudo"
# What runs after root, stubbed: it says what it got.
check_system() { echo "root: server=$SERVER key=$TAILSCALE_KEY version=$VERSION mirror=${PYO_RELEASES:-} ts=$TS_AUTHKEY"; exit 0; }
out=$(PATH="$bin:$PATH" PYO_RELEASES=https://mirror.example bash -c "$(declare -f); $(declare -p SETTINGS $SETTINGS); main \"\$@\"" sandbox.sh "${args[@]}" 2>&1)
check "sudo is asked" "$out" "Pyo needs root. Asking sudo" has
check "the root shell has the options, SETTINGS and PYO_*" "$out" "root: server=sbs_abc123 key=$TAILSCALE_KEY version=0.4.0 mirror=https://mirror.example ts=tskey-auth-x" has

[ "$failed" = 0 ] && echo "sandbox.sh: all checks passed"
exit "$failed"
