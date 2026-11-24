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
json_ok_early() { python3 -c 'import json,sys; json.load(open(sys.argv[1])); print("json")' "$1" 2>&1; }
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

# ── Options on Pyo Cloud: --pyo-url and --command-key instead of --pyo ──
ckey=$(printf 'k%.0s' {1..43})
cloud=(--pyo-url https://agent.pyo.example --server sbs_abc123 --name sbx-1 --code "$code" --tailscale-key tskey-auth-x --command-key "$ckey,$ckey" --version 0.51.1)
out=$( (defaults; ETC=$tmp/etc; parse_args "${cloud[@]}" && echo "parsed $PYO_URL $SERVER $NAME $COMMAND_KEY") 2>&1)
check "the Cloud command parses" "$out" "^parsed https://agent.pyo.example sbs_abc123 sbx-1 $ckey,$ckey$" has
out=$( (defaults; ETC=$tmp/etc; parse_args "${cloud[@]}" --edge-domain sbx.example.com && echo "edge=$EDGE_DOMAIN") 2>&1)
check "and takes an edge domain" "$out" "^edge=sbx.example.com$" has
out=$( (defaults; ETC=$tmp/etc; parse_args "${cloud[@]}" --pyo x:1) 2>&1)
check "--pyo and --pyo-url don't go together" "$out" "--pyo or --pyo-url, not both" has
out=$( (defaults; ETC=$tmp/etc; parse_args --pyo-url https://agent.pyo.example --server sbs_a --name n --code "$code" --version 0.51.1) 2>&1)
check "--pyo-url needs the command key" "$out" "it has --pyo-url, --command-key" has
out=$( (defaults; ETC=$tmp/etc; parse_args "${cloud[@]}" --pyo-url http://agent.pyo.example) 2>&1)
check "Pyo's address is HTTPS" "$out" "isn't Pyo's HTTPS address" has
out=$( (defaults; ETC=$tmp/etc; parse_args "${cloud[@]}" --pyo-url http://127.0.0.1:8080 --dev && echo parsed) 2>&1)
check "or HTTP with --dev" "$out" "^parsed$" has
out=$( (defaults; ETC=$tmp/etc; parse_args "${cloud[@]}" --command-key 'abc;rm -rf') 2>&1)
check "the command key is base64url" "$out" "command key is wrong" has
out=$( (defaults; ETC=$tmp/etc; parse_args "${args[@]}" --command-key "$ckey") 2>&1)
check "--command-key goes with --pyo-url" "$out" "goes with --pyo-url" has
out=$( (defaults; ETC=$tmp/etc; parse_args "${cloud[@]}" --pyo-url https://agent.pyo.example/ && echo "url=$PYO_URL") 2>&1)
check "a trailing slash is dropped" "$out" "^url=https://agent.pyo.example$" has

# ── The release's files ──
defaults
ARCH=x86_64 VERSION=0.4.0
check "the release URL" "$(bundle_url)" "^https://github.com/upsetdev/pyo/releases/download/v0.4.0/pyo-sandbox-linux-x86_64.tar.gz$" has
mkdir -p "$tmp/b"
for f in pyo-sandbox pyo-envd firecracker jailer vmlinux; do echo "$f" >"$tmp/b/$f"; done
tar -czf "$tmp/bundle.tar.gz" -C "$tmp/b" .
sum=$(sha256sum "$tmp/bundle.tar.gz" | cut -d' ' -f1)
WORK=$tmp/work1 BUNDLE=$tmp/bundle.tar.gz BUNDLE_SHA256=$sum
mkdir -p "$WORK"
out=$( (fetch_bundle && ls "$WORK/bundle") 2>&1)
check "a local bundle with its checksum is used" "$out" "vmlinux" has
WORK=$tmp/work1b BUNDLE_SHA256=$(printf '%064d' 0)
mkdir -p "$WORK"
out=$( (fetch_bundle && echo "went on") 2>&1)
check "a local bundle that doesn't match is refused" "$out" "don't match their checksum" has
check "and goes no further" "$out" "went on" lacks
WORK=$tmp/work1c BUNDLE_SHA256="" DEV=1
mkdir -p "$WORK"
out=$( (fetch_bundle && ls "$WORK/bundle") 2>&1)
check "--dev uses a local bundle unchecked" "$out" "vmlinux" has
check "and says so" "$out" "aren't checked \\(--dev\\)" has
out=$( (defaults; ETC=$tmp/etc; parse_args --server sbs_a --name n --code "$code" --pyo x:1 --bundle "$tmp/bundle.tar.gz") 2>&1)
check "--bundle alone is refused" "$out" "--bundle needs a checksum" has
out=$( (defaults; ETC=$tmp/etc; parse_args --server sbs_a --name n --code "$code" --pyo x:1 --bundle "$tmp/bundle.tar.gz" --dev && echo parsed) 2>&1)
check "--bundle with --dev parses" "$out" "^parsed$" has
out=$( (defaults; ETC=$tmp/etc; parse_args --server sbs_a --name n --code "$code" --pyo x:1 --bundle "$tmp/bundle.tar.gz" --bundle-sha256 nothex) 2>&1)
check "--bundle-sha256 is a sha256" "$out" "64 hex digits" has
rm "$tmp/b/jailer" && tar -czf "$tmp/short.tar.gz" -C "$tmp/b" .
WORK=$tmp/work2 BUNDLE=$tmp/short.tar.gz DEV=1
mkdir -p "$WORK"
out=$( (fetch_bundle) 2>&1)
check "a bundle without the jailer is refused" "$out" "have no jailer" has
# A download shows progress, and its failure says what to do, without
# curl's own words.
curl() { echo "curl: (6) Could not resolve host" >&2; return 6; }
WORK=$tmp/work2b BUNDLE="" DEV="" BUNDLE_SHA256=""
mkdir -p "$WORK"
out=$( (fetch_bundle) 2>&1)
check "the release's checksums come first" "$out" "^→ Downloading the release's checksums…$" has
check "a failed download says to run it again" "$out" "Couldn't download release.json of Pyo 0.4.0.*run the same command again\.$" has
WORK=$tmp/work2b2 BUNDLE_SHA256=$(printf '%064d' 0)
mkdir -p "$WORK"
out=$( (fetch_bundle) 2>&1)
check "the download shows progress" "$out" "^→ Downloading the sandbox files…$" has
check "and its failure says to run it again" "$out" "Couldn't download the sandbox files from https://github.com/.*run the same command again\.$" has
# The release: release.json lists the files' checksums, and its signature
# is checked with cosign first.
cp "$tmp/bundle.tar.gz" "$tmp/release-bundle"
good=$(sha256sum "$tmp/release-bundle" | cut -d' ' -f1)
curl() {
  local a o="" url=""
  for a in "$@"; do case "$a" in http*) url=$a ;; esac; done
  for a in "$@"; do
    if [ "$o" = 1 ]; then
      case "$url" in
        */release.json) printf '{\n  "version": "0.4.0",\n  "sandbox": {\n    "x86_64": "%s",\n    "aarch64": "%s"\n  }\n}\n' "$fake_sum" "$(printf '%064d' 1)" >"$a" ;;
        *.sigstore.json) echo '{}' >"$a" ;;
        *) cp "$tmp/release-bundle" "$a" ;;
      esac
      o=""
    fi
    [ "$a" = -o ] && o=1
  done
  return 0
}
cosign() { echo "$*" >"$tmp/cosign-args"; [ -z "${cosign_fails:-}" ] || { echo "none of the expected identities matched" >&2; return 1; }; }
WORK=$tmp/work2c BUNDLE_SHA256="" fake_sum=$good
mkdir -p "$WORK"
out=$( (fetch_bundle && ls "$WORK/bundle") 2>&1)
check "the release's signature is checked" "$out" "^→ Checking the release's signature…$" has
check "with cosign, as install.sh does" "$(cat "$tmp/cosign-args")" "verify-blob --bundle .*release.json.sigstore.json --certificate-identity-regexp \^https://github\\\\.com/upsetdev/pyo/.*--certificate-oidc-issuer https://token.actions.githubusercontent.com .*release.json$" has
check "a release's files that match are used" "$out" "vmlinux" has
WORK=$tmp/work2d fake_sum=$(printf '%064d' 2)
mkdir -p "$WORK"
out=$( (fetch_bundle) 2>&1)
check "a bad checksum says to run it again" "$out" "don't match their checksum. Run the same command again\.$" has
check "and shows no sha256sum output" "$out" "FAILED|WARNING" lacks
WORK=$tmp/work2e fake_sum=$good
mkdir -p "$WORK"
out=$( (cosign_fails=1 fetch_bundle && echo "went on") 2>&1)
check "a bad signature stops it" "$out" "signature doesn't check out" has
check "before the files are used" "$out" "went on" lacks
WORK=$tmp/work2f fake_sum=""
mkdir -p "$WORK"
out=$( (fetch_bundle) 2>&1)
check "a release without the checksums is refused" "$out" "doesn't list the sandbox files' checksum" has
unset -f curl cosign

# gVisor's files, for servers without KVM: checked against release.json's
# "sandboxGvisor" like the sandbox files against "sandbox".
mkdir -p "$tmp/g/gvisor-bin"
echo runsc >"$tmp/g/runsc" && echo sentry >"$tmp/g/gvisor-bin/gvisor_sentry"
tar -czf "$tmp/gvisor.tar.gz" -C "$tmp/g" runsc gvisor-bin
gv_good=$(sha256sum "$tmp/gvisor.tar.gz" | cut -d' ' -f1)
curl() {
  local a o="" url=""
  for a in "$@"; do case "$a" in http*) url=$a ;; esac; done
  echo "$url" >>"$tmp/urls"
  for a in "$@"; do
    if [ "$o" = 1 ]; then
      case "$url" in
        */release.json) printf '{\n  "version": "0.4.0",\n  "sandbox": {\n    "x86_64": "%s"\n  },\n  "sandboxGvisor": {\n    "x86_64": "%s",\n    "aarch64": "%s"\n  }\n}\n' "$(printf '%064d' 3)" "$gv_sum" "$(printf '%064d' 4)" >"$a" ;;
        *.sigstore.json) echo '{}' >"$a" ;;
        *) cp "$tmp/gvisor.tar.gz" "$a" ;;
      esac
      o=""
    fi
    [ "$a" = -o ] && o=1
  done
  return 0
}
cosign() { :; }
defaults
ARCH=x86_64 VERSION=0.4.0 LIB=$tmp/lib WORK=$tmp/work-gv1 gv_sum=$gv_good
mkdir -p "$WORK"
out=$( (add_gvisor && ls "$LIB" "$LIB/gvisor-bin") 2>&1)
check "gVisor comes from the release" "$(cat "$tmp/urls")" "/v0.4.0/pyo-sandbox-gvisor-linux-x86_64.tar.gz$" has
check "and is installed next to its gvisor-bin" "$out" "gvisor_sentry" has
check "with runsc" "$(cat "$LIB/runsc")" "^runsc$" has
check "release_sum reads sandboxGvisor" "$(release_sum sandboxGvisor)" "^$gv_good$" has
check "and still sandbox by default" "$(release_sum)" "^0{63}3$" has
WORK=$tmp/work-gv2 gv_sum=$(printf '%064d' 5)
mkdir -p "$WORK"
out=$( (add_gvisor && echo "went on") 2>&1)
check "gVisor that doesn't match is refused" "$out" "gVisor's files don't match their checksum" has
check "and goes no further" "$out" "went on" lacks
WORK=$tmp/work-gv3 VERSION="" DEV=""
mkdir -p "$WORK"
out=$( (fetch_gvisor) 2>&1)
check "unchecked gVisor needs --dev" "$out" "give --version X, or --dev" has
unset -f curl cosign
rm -f "$tmp/urls"

# ── Detection ──
defaults
WORK=$tmp/work3 JAIL_UID=998 JAIL_GID=997 BIN=$tmp/pyo-sandbox LIB=$tmp/lib-detect
mkdir -p "$WORK"
add_gvisor() { echo "Adding gVisor"; }
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
check "Firecracker on KVM is reported" "$out" "^● Firecracker on KVM. A test VM booted in 73 ms." has
check "the detection shows progress" "$out" "^→ Checking what this server can run…$" has
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
out=$( (detect) 2>&1)
check "gVisor is reported" "$out" "gVisor, because this server has no usable KVM" has
check "a server without KVM gets gVisor first" "$out" "^Adding gVisor$" has
check "the detection gets runsc's path" "$(detect_config)" '"runsc": "[^"]*/runsc"' has
mkdir -p "$LIB" && touch "$LIB/runsc" && chmod +x "$LIB/runsc"
check "gVisor once is enough" "$( (detect) 2>&1)" "Adding gVisor" lacks
rm -rf "$LIB"
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
check "joining the tailnet shows progress" "$out" "^→ Joining your tailnet…$" has
check "and hides Tailscale's own output" "$out" "advertising" lacks
check "and gets this server's address" "$out" "^ip=100.64.0.9 pyo=100.64.0.1:5006$" has
rm -f "$tmp/ts-up"
out=$( (ts_fails=1 join_tailnet) 2>&1)
check "a failed join says to run a fresh command" "$out" "didn't join your tailnet. Copy a fresh command from Pyo and run it again\.$" has
out=$( (TS_AUTHKEY="" join_tailnet) 2>&1)
check "no key, no join" "$out" "Not on your tailnet yet" has
# On Pyo Cloud: one of Pyo's on the team's tailnet, and its names there.
rm -f "$tmp/ts-up"
defaults
NAME=sbx-1 TS_AUTHKEY=tskey-auth-x PYO_URL=https://agent.pyo.example
ts_status='{
  "Version": "1.90.0",
  "Self": {
    "HostName": "sbx-1",
    "DNSName": "sbx-1.tail1234.ts.net.",
    "Tags": [
      "tag:pyo"
    ]
  },
  "CurrentTailnet": {
    "MagicDNSSuffix": "tail1234.ts.net"
  }
}'
tailscale() {
  case "$1" in
    ip) [ ! -f "$tmp/ts-up" ] || echo 100.64.0.9 ;;
    up) echo "$*" >"$tmp/ts-up-args"; touch "$tmp/ts-up" ;;
    status) echo "$ts_status" ;;
    cert) echo "$*" >"$tmp/ts-cert-args"; [ -z "${cert_fails:-}" ] || { echo "500 Internal Server Error: your Tailscale account does not support getting TLS certs" >&2; return 1; } ;;
  esac
  return 0
}
out=$( (join_tailnet && echo "ip=$TS_IP host=$TS_HOST name=$TS_NAME") 2>&1)
check "Cloud: it joins the tailnet with the key and its name" "$(cat "$tmp/ts-up-args")" "^up --authkey tskey-auth-x --hostname sbx-1 " has
check "Cloud: and gets its names there" "$out" "^ip=100.64.0.9 host=sbx-1 name=sbx-1.tail1234.ts.net$" has
ts_status='{"Self": {"HostName": "laptop", "DNSName": "laptop.tail9999.ts.net."}}'
out=$( (join_tailnet && echo "went on") 2>&1)
check "Cloud: a machine on a tailnet as someone else's isn't moved" "$out" "on a tailnet already, but not as one of Pyo's servers" has
check "Cloud: and goes no further" "$out" "went on" lacks
ETC=$tmp/etc-cert TS_NAME=sbx-1.tail1234.ts.net
out=$( (tailnet_cert) 2>&1)
check "Cloud: the certificate for its tailnet name" "$(cat "$tmp/ts-cert-args")" "^cert --cert-file $ETC/tailnet/cert.pem --key-file $ETC/tailnet/key.pem sbx-1.tail1234.ts.net$" has
check "Cloud: shows progress" "$out" "^→ Getting an HTTPS certificate for sbx-1.tail1234.ts.net…$" has
check "Cloud: then that it got it" "$out" "^✓ Got an HTTPS certificate for sbx-1.tail1234.ts.net$" has
check "Cloud: its folder is root's only" "$(stat -c %a "$ETC/tailnet")" "^700$" has
out=$( (cert_fails=1 tailnet_cert && echo "went on") 2>&1)
check "Cloud: no certificate says what to do" "$out" "Turn on HTTPS certificates in your tailnet's DNS settings, then run the same command again\.$" has
check "Cloud: and stops" "$out" "went on" lacks
CERT_UNIT=$tmp/pyo-sandbox-cert
write_cert_units
check "Cloud: a timer renews the certificate" "$(cat "$CERT_UNIT.timer")" "^OnUnitActiveSec=12h$" has
check "Cloud: with tailscale cert for its name" "$(cat "$CERT_UNIT.service")" "^ExecStart=/usr/bin/env tailscale cert --cert-file $ETC/tailnet/cert.pem --key-file $ETC/tailnet/key.pem sbx-1.tail1234.ts.net$" has
check "Cloud: its key stays root's" "$(cat "$CERT_UNIT.service")" "^UMask=0077$" has
unset -f systemctl tailscale tailnet_lock_wait

# ── Joining Pyo on Pyo Cloud: the daemon joins over HTTPS ──
sleep() { :; }
defaults
ETC=$tmp/etc-out WORK=$tmp/work-out BIN=$tmp/pyo-sandbox-out PYO_URL=https://agent.pyo.example SERVER=sbs_abc123 NAME=sbx-1
CODE=$code COMMAND_KEY="$ckey" TS_IP=100.64.0.9 TS_HOST=sbx-1 TS_NAME=sbx-1.tail1234.ts.net JAIL_UID=998 JAIL_GID=997
mkdir -p "$WORK"
printf '{\n  "runtime": "firecracker"\n}\n' >"$WORK/capability.json"
# The daemon's join, stubbed: what it got, then its config, or a refusal.
cat >"$BIN" <<SH
#!/usr/bin/env bash
echo "\$*" >"$tmp/join-args"
f=\${@: -1}
stat -c %a "\$f" >"$tmp/join-mode"
cp "\$f" "$tmp/join-asked.json"
if [ -n "\${JOIN_FAILS:-}" ]; then
  echo "2026-11-24T10:00:00Z  WARN pyo_sandbox: not listed yet, asking again" >&2
  echo "Error: This join code expired. Make a new one in Pyo." >&2
  exit 1
fi
echo '{"serverId": "sbs_abc123", "outbound": {"pyoUrl": "https://agent.pyo.example"}}' >"$ETC/config.json"
SH
chmod +x "$BIN"
out=$( (join_outbound) 2>&1)
check "Cloud: the daemon joins with the join file" "$(cat "$tmp/join-args")" "^join --config $ETC/config.json --join $ETC/join.json$" has
check "Cloud: the code isn't in its arguments" "$(cat "$tmp/join-args")" "$code" lacks
check "Cloud: the join file is root's only" "$(cat "$tmp/join-mode")" "^600$" has
check "Cloud: and is gone after" "$(ls "$ETC")" "join.json" lacks
check "Cloud: it's valid JSON" "$(json_ok_early "$tmp/join-asked.json")" "^json$" has
check "Cloud: with Pyo, the server, code and command key" "$(cat "$tmp/join-asked.json")" "\"pyoUrl\": \"https://agent.pyo.example\", \"serverId\": \"sbs_abc123\", \"code\": \"$code\", \"commandKeys\": \"$ckey\"" has
check "Cloud: and its tailnet address and names" "$(cat "$tmp/join-asked.json")" "\"address\": \"100.64.0.9\", \"hostname\": \"sbx-1\", \"tailnetName\": \"sbx-1.tail1234.ts.net\", \"jailUid\": 998, \"jailGid\": 997, \"capability\": \{" has
check "Cloud: joining shows progress" "$out" "^→ Joining Pyo as sbx-1…$" has
check "Cloud: then that it joined" "$out" "^✓ Joined Pyo as sbx-1$" has
check "Cloud: it's in outbound mode from then on" "$( (PYO_URL=""; outbound && echo outbound) 2>&1)" "^outbound$" has
rm -f "$ETC/config.json"
out=$( (JOIN_FAILS=1 join_outbound && echo "went on") 2>&1)
check "Cloud: a refusal shows the daemon's last line" "$out" $'^✗ Joining Pyo as sbx-1\nError: This join code expired. Make a new one in Pyo.$' has
check "Cloud: and none of its log" "$out" "WARN|pyo_sandbox" lacks
check "Cloud: and stops" "$out" "went on" lacks
check "Cloud: the join file is gone after a refusal too" "$(ls "$ETC")" "join.json" lacks
check "Cloud: self-hosted isn't outbound" "$( (PYO_URL=""; outbound || echo direct) 2>&1)" "^direct$" has
unset -f sleep

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
check "joining Pyo shows progress" "$out" "^→ Joining Pyo as sbx-1…$" has
check "then that it joined" "$out" "^✓ Joined Pyo as sbx-1$" has
# Pyo out of reach: the wait ends after 10 minutes, without curl's words.
sleep() { SECONDS=$((SECONDS + ${1%.*})); }
fake_body='curl: (7) Failed to connect' fake_code=000
out=$( (redeem) 2>&1)
check "an unreachable Pyo says what to do" "$out" "didn't answer over your tailnet in 10 minutes. Check that Pyo is running, then run the same command again\.$" has
check "and not curl's error" "$out" "curl:" lacks
check "the wait shows every 20 seconds in a log" "$out" "^→ Joining Pyo as sbx-1… 0:20$" has
check "then that it failed" "$out" $'^✗ Joining Pyo as sbx-1\nError: Pyo didn\'t answer' has
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
check "a daemon that won't start shows progress" "$out" "^→ Starting the sandbox daemon…$" has
check "and that it failed" "$out" "^✗ Starting the sandbox daemon$" has
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
# Outbound: the HTTPS port on the tailnet too.
ufw_state=active
echo '{"outbound": {}}' >"$ETC/config.json"
rm -f "$tmp/ufw"
firewall
check "ufw: Cloud, HTTPS on the tailnet" "$(cat "$tmp/ufw")" "ufw allow in on tailscale0 to any port 7443 proto tcp" has
rm -f "$ETC/config.json" "$tmp/ufw"
firewall
check "ufw: self-hosted, no 7443" "$(cat "$tmp/ufw")" "7443" lacks
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
