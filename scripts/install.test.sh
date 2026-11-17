#!/usr/bin/env bash
# deploy/install.sh's functions, without a machine to install on: its
# functions are sourced (all but the last line, which runs it) and kubectl
# is stubbed. Run: bash scripts/install.test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

bash -n deploy/install.sh
# shellcheck source=/dev/null
source <(sed '$d' deploy/install.sh)
defaults

failed=0
check() { # $1: what, $2: output, $3: grep -E pattern, $4: "has" or "lacks"
  if grep -qE "$3" <<<"$2"; then
    [ "$4" = has ] && return 0
  else
    [ "$4" = lacks ] && return 0
  fi
  printf 'FAIL: %s (%s %s)\n%s\n' "$1" "$4" "$3" "$2" >&2
  failed=1
}

# The pyo-admin Secret is there, as it stays after the first sign-in. Pyo
# says whether the first admin still has to sign in ($answer: its answer,
# or none).
kc() {
  case "$*" in *"jsonpath="*) printf 'c2VjcmV0LWFnYWlu' ;; esac # "secret-again"
  return 0
}
host_key() { echo k3y; }
curl() {
  case "$*" in *"/install/first-sign-in?key=k3y"*) [ -n "$answer" ] && echo "$answer" ;; *) return 7 ;; esac
}

ADMIN_PASSWORD=s3cret
answer='{"pending":false}'
out=$(print_sign_in)
check "a fresh install prints its login" "$out" "admin@pyo.local" has
check "a fresh install prints its password" "$out" "Password: s3cret" has

ADMIN_PASSWORD=""
out=$(print_sign_in)
check "an update says nothing of the first sign-in" "$out" "admin@pyo.local|First sign-in" lacks
check "an update says how to update again" "$out" "run the same command again" has

unset ADMIN_PASSWORD
out=$(print_sign_in)
check "nothing to update says nothing of the first sign-in" "$out" "admin@pyo.local|First sign-in" lacks

# Run again before anyone signed in (an install stopped while it waited):
# the same login again.
answer='{"pending":true}'
out=$(print_sign_in)
check "not signed in yet: the login again" "$out" "Email: +admin@pyo\.local" has
check "not signed in yet: its password" "$out" "Password: secret-again" has
ADMIN_PASSWORD=""
out=$(print_sign_in)
check "not signed in yet, on an update too" "$out" "Password: secret-again" has

# Pyo can't say (older, or not answering): where the login is, never the
# password.
ADMIN_PASSWORD=""
answer=""
out=$(print_sign_in)
check "unknown: the email" "$out" "admin@pyo\.local" has
check "unknown: where the password is" "$out" "get secret pyo-admin" has
check "unknown: never the password" "$out" "secret-again|Password:" lacks
unset ADMIN_PASSWORD
unset -f curl host_key

# PYO_TELEMETRY=off: the fresh install's Pyo gets it (the pyo-install
# ConfigMap), and the installer says so.
kc() {
  echo "kc $*" >>"$calls"
  case "$*" in *"create configmap"*) echo "kind: ConfigMap" ;; esac
}
calls=$(mktemp)
trap 'rm -f "$calls"' EXIT
TELEMETRY=off
install_settings
check "telemetry off writes pyo-install" "$(cat "$calls")" "create configmap pyo-install --from-literal=telemetry=off" has
check "and applies it" "$(cat "$calls")" "kc apply -f -" has
# The count is silent either way: the docs say what it sends.
curl() { echo "curl ${*: -1}" >>"$calls"; }
: >"$calls"
out=$(count_install 2>&1)
check "telemetry off counts nothing" "$(cat "$calls")" "api\.pyo\.dev" lacks
check "and says nothing" "$out" "." lacks
TELEMETRY=""
R_VERSION=9.9.9
: >"$calls"
out=$(count_install 2>&1)
check "telemetry on counts the install" "$(cat "$calls")" "curl https://api\.pyo\.dev/v1/installs" has
check "and says nothing either" "$out" "." lacks
unset -f curl
TELEMETRY=off
for v in OFF false 0 no; do
  TELEMETRY=$v
  telemetry_off || check "PYO_TELEMETRY=$v is off" "on" "^off$" has
done
: >"$calls"
TELEMETRY=""
install_settings
check "telemetry on removes pyo-install" "$(cat "$calls")" "delete configmap pyo-install --ignore-not-found" has
check "and writes nothing" "$(cat "$calls")" "create configmap" lacks
telemetry_off && check "no PYO_TELEMETRY is on" "off" "^on$" has

# The release lookup: api.pyo.dev first, then GitHub, each a few tries,
# and a mirror alone when there is one. curl is stubbed: $answers says which
# hosts answer, and $asked gets each address with the headers sent.
sleep() { :; }
curl() {
  local url=${*: -1} a headers=""
  for a in "$@"; do case "$a" in *:\ *) headers="$headers [$a]" ;; esac; done
  echo "$url$headers" >>"$asked"
  case " $answers " in
    *" ${url#*//}"*) echo '{"tag_name": "v9.9.9", "assets": []}' ;;
    *)
      case "$url" in */tags/*) echo "curl: (22) The requested URL returned error: 404" >&2 ;; *) echo "curl: (22) The requested URL returned error: 403" >&2 ;; esac
      return 22
      ;;
  esac
}
asked=$(mktemp)
trap 'rm -f "$calls" "$asked"' EXIT
gh=api.github.com/repos/upsetdev/pyo/releases/latest
pyo=api.pyo.dev/v1/releases/latest

defaults
answers="$pyo $gh"
: >"$asked"
out=$(release_meta)
check "api.pyo.dev answers" "$out" '"tag_name": "v9.9.9"' has
check "api.pyo.dev is asked first, and alone" "$(cat "$asked")" "^https://$pyo" has
check "GitHub isn't asked then" "$(cat "$asked")" "api\.github" lacks
check "no install ID or version goes along" "$(cat "$asked")" "\\[(Pyo|X-|User-Agent)" lacks

answers="$gh"
: >"$asked"
out=$(release_meta 2>/dev/null)
check "GitHub answers when api.pyo.dev doesn't" "$out" '"tag_name": "v9.9.9"' has
check "api.pyo.dev gets three tries first" "$(grep -c "$pyo" "$asked")" "^3$" has
check "then GitHub" "$(tail -n1 "$asked")" "^https://$gh" has

answers=""
: >"$asked"
if err=$(release_meta 2>&1 >/dev/null); then check "nothing answers" "answered" "^failed$" has; fi
check "three tries each" "$(wc -l <"$asked")" "^6$" has
check "says why api.pyo.dev failed" "$err" "$pyo: curl: \\(22\\).*error: 403" has
check "says why GitHub failed" "$err" "$gh: curl: \\(22\\).*error: 403" has
WORK=$(mktemp -d)
out=$( (fetch_release) 2>&1 || true)
rm -rf "$WORK"
check "the installer shows where and why" "$out" "$gh: curl.*error: 403" has
check "and stops" "$out" "Error: Couldn't look up the latest Pyo release" has

SOURCE=https://mirror.example/pyo
answers="mirror.example/pyo/releases/latest $pyo"
: >"$asked"
out=$(release_meta)
check "a mirror answers" "$out" '"tag_name"' has
check "a mirror is the only place asked" "$(cat "$asked")" "pyo\.dev|api\.github" lacks

defaults
VERSION=1.2.3
answers=""
: >"$asked"
WORK=$(mktemp -d)
out=$( (fetch_release) 2>&1 || true)
rm -rf "$WORK"
check "--version asks GitHub for that release" "$(cat "$asked")" "^https://api.github.com/repos/upsetdev/pyo/releases/tags/v1.2.3" has
check "once, when it isn't there" "$(wc -l <"$asked")" "^1$" has
check "and says so" "$out" "There's no release 1.2.3" has
# ── The addresses at the end (addresses.sh, as host_units writes it) ──
# The machine is stubbed: its cards (ip), its cloud's metadata and Pyo
# (curl), Tailscale, and cert-manager's view of the HTTPS address's
# certificate (k3s kubectl). Sleeping only moves the clock.
lib=$(mktemp)
sed -n "/tee \/usr\/local\/lib\/pyo\/addresses.sh >\/dev\/null <<'ADDR'$/,/^ADDR$/p" deploy/install.sh | sed '1d;$d' >"$lib"
bash -n "$lib"
check "addresses.sh is in host_units" "$(wc -l <"$lib")" "^[0-9]{3}$" has
# shellcheck source=/dev/null
. "$lib"
PYO_ADDRESSES_SH=$lib
sleep() { SECONDS=$((SECONDS + ${1%.*})); }
id() { echo 0; }
hostname() { if [ "${1:-}" = -I ]; then echo "$ips"; else echo VM-1; fi; }
ip() {
  case "$*" in
    "-o -4 addr show") [ -n "$cards" ] && echo "$cards" ;;
    "-4 route show default") [ -z "$route" ] || echo "default via 10.0.0.1 dev $route proto dhcp src 10.0.0.9 metric 100" ;;
  esac
}
tailscale() { [ "$*" = "ip -4" ] && [ -n "$ts_ip" ] && echo "$ts_ip"; }
# cloud: which metadata service answers (with cloud_ip); meta: Pyo's
# /api/v1/meta; ts_answers: Pyo answers on the Tailscale IP.
curl() {
  local url=${*: -1}
  echo "$*" >>"$asked"
  case "$url" in
    http://127.0.0.1:8000/api/v1/meta) [ -n "$meta" ] && echo "$meta" ;;
    http://100.*:8000/api/v1/meta) [ -n "$ts_answers" ] ;;
    http://169.254.169.254/latest/api/token) [ "$cloud" = aws ] && printf tok ;;
    http://169.254.169.254/latest/meta-data/public-ipv4) [ "$cloud" = aws ] && [[ $* == *"X-aws-ec2-metadata-token: tok"* ]] && printf '%s' "$cloud_ip" ;;
    *computeMetadata*) [ "$cloud" = gcp ] && [[ $* == *"Metadata-Flavor: Google"* ]] && printf '%s' "$cloud_ip" ;;
    *169.254.169.254/metadata/instance*) [ "$cloud" = azure ] && [[ $* == *"Metadata: true"* ]] && printf '%s' "$cloud_ip" ;;
    http://100.100.100.200/latest/api/token) return 22 ;;
    http://100.100.100.200/latest/meta-data/eipv4) [ "$cloud" = alibaba ] && printf '%s' "$cloud_ip" ;;
    http://169.254.0.23/latest/meta-data/public-ipv4) [ "$cloud" = tencent ] && printf '%s' "$cloud_ip" ;;
    http://169.254.42.42/conf) [ "$cloud" = scaleway ] && printf 'PRIVATE_IP=10.1.2.3\nPUBLIC_IP_ADDRESS=%s\n' "$cloud_ip" ;;
    *) return 7 ;;
  esac || return 22
}
# cert: the certificate as the jsonpath reads it ("" none); the challenge
# is turned down from look number $fail_after on (counted in $polls, a
# file: the reads run in subshells).
k3s() {
  shift
  case "$*" in
    *"get certificate pyo-tls---dashboard"*)
      echo x >>"$polls"
      [ -n "$cert" ] && echo "$cert" ;;
    *challenges.acme*)
      # An older try's, turned down: not this certificate's.
      [ -z "$cert" ] || echo "2026-11-17T08:00:00Z ${cert%% *} invalid"
      [ -n "$cert" ] && [ "$(wc -l <"$polls")" -ge "${fail_after:-999}" ] && echo "2026-11-17T09:00:01Z ${cert%% *} invalid"
      return 0 ;;
    *"delete certificate pyo-tls---dashboard"*)
      echo deleted >>"$asked"
      cert="${cert%% *} False- DoesNotExist- 2026-11-17T09:30:00Z " ;;
    *"get node vm-1"*) echo "$agent_ip" ;;
  esac
}
asked=$(mktemp) polls=$(mktemp)
reset() {
  unset ADDR_IP_KNOWN ADDR_IP ADDR_CERT_STATE ADDR_CERT_HOST ADDR_WAITED STARTING
  cards="" route="" ips="" ts_ip="" ts_answers="" cloud="" cloud_ip="" meta='{"version":"9.9.9"}' cert="" agent_ip="" fail_after="" failed_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  : >"$asked"
  : >"$polls"
}
lo='1: lo    inet 127.0.0.1/8 scope host lo\       valid_lft forever preferred_lft forever'
k3s_cards='5: cni0    inet 10.42.0.1/24 brd 10.42.0.255 scope global cni0\       valid_lft forever preferred_lft forever
6: flannel.1    inet 10.42.0.0/32 scope global flannel.1\       valid_lft forever preferred_lft forever'
ts_card='3: tailscale0    inet 100.101.67.2/32 scope global tailscale0\       valid_lft forever preferred_lft forever'

# A Tencent Cloud server: a private address on its card, the public IP in
# front of it (1:1 NAT) from the metadata, on a tailnet already, the HTTPS
# address issued. The block as the user sees it, lined up.
reset
cards="$lo
2: eth0    inet 10.3.8.13/22 brd 10.3.11.255 scope global eth0\       valid_lft forever preferred_lft forever
$ts_card
$k3s_cards"
route=eth0 cloud=tencent cloud_ip=43.156.1.2 ts_ip=100.101.67.2 ts_answers=1
cert="pyo.43.156.1.2.nip.io True- Issued- 2026-11-17T09:00:00Z "
out=$(pyo_addresses 300)
want=$'\nOpen Pyo:\n  ➜  Public:   https://pyo.43.156.1.2.nip.io\n  ➜  Tailnet:  http://100.101.67.2:8000\n  ➜  Network:  http://10.3.8.13:8000\n  ➜  Local:    http://localhost:8000'
same() { if [ "$1" = "$2" ]; then echo same; else printf '%s\n--- wanted:\n%s\n' "$1" "$2"; fi; }
check "a cloud server behind NAT: the whole block" "$(same "$out" "$want")" "^same$" has
check "issued: no wait" "$out" "Waiting" lacks
check "never plain HTTP on the public IP" "$out" "http://43\.156" lacks
check "no k3s address" "$out" "10\.42\." lacks
check "no Tailscale hint with HTTPS" "$out" "set up Tailscale" lacks
check "no colors off a terminal" "$out" $'\033' lacks

# Let's Encrypt couldn't reach it: the address still, with what to do.
reset
cards="$lo
2: eth0    inet 10.3.8.13/22 brd 10.3.11.255 scope global eth0"
route=eth0 cloud=tencent cloud_ip=43.156.1.2
cert="pyo.43.156.1.2.nip.io False- Failed- 2026-11-17T09:00:00Z $failed_at"
out=$(pyo_addresses 300)
check "blocked: the public address and the ports to open" "$out" "^  ➜  Public:   https://pyo\.43\.156\.1\.2\.nip\.io  \(allow ports 80 and 443 in your cloud firewall\)$" has
check "blocked: failed already, no wait" "$out" "Waiting" lacks
check "blocked: no Tailscale hint" "$out" "set up Tailscale" lacks
check "blocked: not on a tailnet, no Tailnet line" "$out" "Tailnet:" lacks

# Run again after opening the ports: a try refused 15 minutes ago or more
# is made again now, and waited for. Within 15 minutes, not.
reset
cards="2: eth0    inet 10.3.8.13/22 scope global eth0"
route=eth0 cloud=tencent cloud_ip=43.156.1.2
failed_at=$(date -u -d '-20 minutes' +%Y-%m-%dT%H:%M:%SZ)
cert="pyo.43.156.1.2.nip.io False- Failed- 2026-11-17T09:00:00Z $failed_at"
out=$(pyo_addresses 300)
check "refused a while ago: tried again" "$(cat "$asked")" "^deleted$" has
check "and waited for" "$out" "^→ Checking the HTTPS address…$" has
reset
cards="2: eth0    inet 10.3.8.13/22 scope global eth0"
route=eth0 cloud=tencent cloud_ip=43.156.1.2
failed_at=$(date -u -d '-5 minutes' +%Y-%m-%dT%H:%M:%SZ)
cert="pyo.43.156.1.2.nip.io False- Failed- 2026-11-17T09:00:00Z $failed_at"
out=$(pyo_addresses 300)
check "refused just now: not again" "$(cat "$asked")" "^deleted$" lacks
check "and not waited for" "$out" "Checking" lacks
out=$(pyo_addresses)
check "no wait asked: never tried again" "$(cat "$asked")" "^deleted$" lacks

# A fresh install: the certificate on its way, then Let's Encrypt turns its
# challenge down. The wait stops right then, well before 90 seconds.
reset
cards="$lo
2: ens5    inet 172.31.5.10/20 brd 172.31.15.255 scope global dynamic ens5
4: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0"
route=ens5 cloud=aws cloud_ip=3.120.1.2
cert="pyo.3.120.1.2.nip.io False- DoesNotExist- 2026-11-17T09:00:00Z " fail_after=8
t0=$SECONDS
out=$(pyo_addresses 300; echo "took $((SECONDS - t0))")
check "AWS: the public IP from IMDSv2" "$out" "https://pyo\.3\.120\.1\.2\.nip\.io  \(allow ports 80 and 443" has
check "turned down: the wait stops at once" "$(sed -n 's/^took //p' <<<"$out")" "^1[0-9]$" has
check "says it checks" "$out" "^→ Checking the HTTPS address…$" has
check "and that it did" "$out" "^✓ Checked the HTTPS address 0:1[0-9]$" has
check "AWS: the private address, not docker's" "$out" "^  ➜  Network:  http://172\.31\.5\.10:8000$" has
check "no docker bridge" "$out" "172\.17\." lacks

# The certificate isn't there yet after 90 seconds: it comes by itself.
reset
cards="$lo
2: ens4    inet 10.128.0.2/32 scope global dynamic ens4"
route=ens4 cloud=gcp cloud_ip=34.72.1.2
t0=$SECONDS
out=$(pyo_addresses 300; echo "took $((SECONDS - t0))")
check "Google Cloud: the public IP from the metadata" "$out" "^  ➜  Public:   https://pyo\.34\.72\.1\.2\.nip\.io  \(ready in a few minutes\)$" has
check "90 seconds at most" "$(sed -n 's/^took //p' <<<"$out")" "^9[0-3]$" has
check "every 20 seconds in a log" "$out" "^→ Checking the HTTPS address… 1:20$" has
check "then it goes on by itself" "$out" "^● Checking the HTTPS address 1:3[0-3]$" has

# Issued while it waits.
reset
cards="$lo
2: eth0    inet 10.0.0.4/24 scope global eth0"
route=eth0 cloud=azure cloud_ip=20.1.2.3
cert="pyo.20.1.2.3.nip.io False- DoesNotExist- 2026-11-17T09:00:00Z "
k3s_was=$(declare -f k3s)
k3s() { shift; case "$*" in *"get certificate"*) echo x >>"$polls"; if [ "$(wc -l <"$polls")" -ge 4 ]; then echo "pyo.20.1.2.3.nip.io True- Issued- 2026-11-17T09:00:00Z "; else echo "$cert"; fi ;; esac; }
out=$(pyo_addresses 300)
eval "$k3s_was"
check "Azure: issued while it waits" "$out" "^  ➜  Public:   https://pyo\.20\.1\.2\.3\.nip\.io$" has

# Alibaba Cloud and Scaleway answer too. Garbage or private answers don't count.
for c in alibaba:47.88.1.2 scaleway:51.15.1.2; do
  reset
  cloud=${c%%:*} cloud_ip=${c#*:}
  check "$cloud: the public IP from the metadata" "$(addr_cloud_ip)" "^${cloud_ip//./\\.}$" has
done
for junk in "<html>404</html>" 999.1.2.3 10.3.8.13 100.101.67.2 ""; do
  reset
  cloud=tencent cloud_ip=$junk
  check "metadata answering \"$junk\" is no public IP" "$(addr_cloud_ip)" "." lacks
done
reset
t0=$SECONDS
addr_cloud_ip >/dev/null
check "every cloud asked at once" "$(grep -c 169.254 "$asked")" "^[4-9]$" has
check "each for a second at most" "$(grep 'curl\|-m' "$asked" | grep -vc -- '-m 1 ')" "^0$" has

# A Hetzner server: the public IP on its own card, and a private network.
reset
cards="$lo
2: eth0    inet 203.0.113.7/32 scope global dynamic eth0
3: ens10    inet 10.0.0.2/32 brd 10.0.0.2 scope global dynamic ens10
$k3s_cards"
route=eth0
cert="pyo.203.0.113.7.nip.io True- Issued- 2026-11-17T09:00:00Z "
out=$(pyo_addresses 300)
check "a public card: its address, no metadata asked" "$(cat "$asked")" "169\.254|100\.100" lacks
check "a public card: HTTPS on it" "$out" "^  ➜  Public:   https://pyo\.203\.0\.113\.7\.nip\.io$" has
check "a public card: the private network's address" "$out" "^  ➜  Network:  http://10\.0\.0\.2:8000$" has
check "a public card: never plain HTTP on it" "$out" "http://203\." lacks

# A laptop with docker, k3s, multipass, libvirt and LXD bridges, on a
# tailnet where Pyo doesn't answer on :8000: just its Wi-Fi address.
reset
cards="$lo
2: wlp0s20f3    inet 192.168.1.23/24 brd 192.168.1.255 scope global dynamic wlp0s20f3
4: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0
7: br-3f2a9c1e    inet 172.18.0.1/16 brd 172.18.255.255 scope global br-3f2a9c1e
$k3s_cards
8: mpqemubr0    inet 10.97.0.1/24 brd 10.97.0.255 scope global mpqemubr0
9: virbr0    inet 192.168.122.1/24 brd 192.168.122.255 scope global virbr0
10: lxdbr0    inet 10.10.10.1/24 scope global lxdbr0
11: veth12ab@if10    inet 10.42.0.9/32 scope global veth12ab
$ts_card"
route=wlp0s20f3 ts_ip=100.101.67.2
out=$(pyo_addresses 300)
want=$'\nOpen Pyo:\n  ➜  Network:  http://192.168.1.23:8000\n  ➜  Local:    http://localhost:8000\nFor an HTTPS address, set up Tailscale in Pyo.'
check "a laptop: one Network line, the hint" "$(same "$out" "$want")" "^same$" has
check "a laptop: no wait" "$out" "Waiting" lacks
check "a laptop: no public line" "$out" "Public" lacks
# Pyo answers on its Tailscale address.
ts_answers=1
out=$(pyo_addresses 300)
check "a laptop on a tailnet: the Tailnet line" "$out" "^  ➜  Tailnet:  http://100\.101\.67\.2:8000$" has
check "a laptop on a tailnet: no Tailscale hint" "$out" "set up Tailscale" lacks
# Pyo's own name on the tailnet comes first.
meta='{"version":"9.9.9","publicUrl":"https://pyo.tail1234.ts.net"}'
out=$(pyo_addresses 300)
check "Pyo's tailnet name" "$out" "^  ➜  Tailnet:  https://pyo\.tail1234\.ts\.net$" has
check "only one Tailnet line" "$(grep -c Tailnet <<<"$out")" "^1$" has

# No ip command: hostname -I, without k3s's or docker's addresses.
reset
ips="10.0.2.15 10.42.0.0 10.42.0.1 172.17.0.1 100.101.67.2 fd7a:115c::1"
out=$(pyo_addresses 300)
check "hostname -I: the network address" "$out" "^  ➜  Network:  http://10\.0\.2\.15:8000$" has
check "hostname -I: one Network line" "$(grep -c Network <<<"$out")" "^1$" has

# Oracle Cloud's metadata doesn't say: what Pyo's agent found does.
reset
cards="2: enp0s6    inet 10.0.0.37/24 scope global enp0s6"
route=enp0s6 agent_ip=129.146.1.2
cert="pyo.129.146.1.2.nip.io True- Issued- 2026-11-17T09:00:00Z "
check "the agent's cloud IP" "$(addr_public_ip)" "^129\.146\.1\.2$" has

# Pyo is on the tailnet and an admin closed the public address: no wait,
# no Public line.
reset
cards="2: eth0    inet 10.3.8.13/22 scope global eth0"
route=eth0 cloud=tencent cloud_ip=43.156.1.2
meta='{"version":"9.9.9","publicUrl":"https://pyo.tail1234.ts.net"}'
out=$(pyo_addresses 300)
check "closed: no wait" "$out" "Waiting" lacks
check "closed: no Public line" "$out" "Public" lacks

# Run again with nothing to update: no wait, the addresses as they are.
reset
cards="2: eth0    inet 10.3.8.13/22 scope global eth0"
route=eth0 cloud=tencent cloud_ip=43.156.1.2
cert="pyo.43.156.1.2.nip.io False- DoesNotExist- 2026-11-17T09:00:00Z "
out=$(pyo_addresses)
check "no wait asked: none" "$out" "Waiting" lacks
check "on its way" "$out" "\(ready in a few minutes\)$" has

# Still starting.
reset
STARTING=1
dashboard_up() { return 1; }
out=$(pyo_addresses)
check "still starting: said first" "$(sed -n 2p <<<"$out")" "^● Pyo is still starting\. It opens at these addresses in a minute or two\.$" has
unset -f dashboard_up

# Colors on a terminal: a green arrow, the label in bold, the address in
# cyan. NO_COLOR or no terminal: plain.
addr_color_was=$(declare -f addr_color)
addr_color() { [ -z "${NO_COLOR:-}" ]; }
out=$(addr_line Public https://pyo.43.156.1.2.nip.io "allow ports 80 and 443 in your cloud firewall")
check "colors" "$out" $'^  \033\\[32m➜\033\\[0m  \033\\[1mPublic:\033\\[0m   \033\\[36mhttps://pyo\\.43\\.156\\.1\\.2\\.nip\\.io\033\\[0m  \\(allow ports' has
out=$(NO_COLOR=1 addr_line Local http://localhost:8000)
check "NO_COLOR" "$out" "^  ➜  Local:    http://localhost:8000$" has
eval "$addr_color_was"
out=$(addr_line Tailnet http://100.101.67.2:8000)
check "no terminal: plain" "$out" $'\033' lacks

# pyo-reset-login's links: only addresses that work now.
reset
cards="2: eth0    inet 10.3.8.13/22 scope global eth0"
route=eth0 cloud=tencent cloud_ip=43.156.1.2
cert="pyo.43.156.1.2.nip.io False- Failed- 2026-11-17T09:00:00Z $failed_at"
out=$(addr_print /reset/abc)
check "links: not the blocked address" "$out" "Public" lacks
check "links: the network one" "$out" "^  ➜  Network:  http://10\.3\.8\.13:8000/reset/abc$" has
cert="pyo.43.156.1.2.nip.io True- Issued- 2026-11-17T09:00:00Z "
unset ADDR_CERT_STATE
out=$(addr_print /reset/abc)
check "links: the issued one" "$out" "^  ➜  Public:   https://pyo\.43\.156\.1\.2\.nip\.io/reset/abc$" has

# The steps' last line, before the addresses.
reset
cards="2: eth0    inet 10.3.8.13/22 scope global eth0"
route=eth0
out=$(pyo_addresses 0 "Pyo 9.9.9 is installed")
check "the last step line" "$(sed -n 1p <<<"$out")" "^✓ Pyo 9\.9\.9 is installed$" has
check "then the addresses" "$(sed -n 3p <<<"$out")" "^Open Pyo:$" has
unset -f curl hostname ip tailscale k3s id sleep
rm -f "$lib" "$asked" "$polls"

# quietly: the output of what it runs only when that fails.
out=$(quietly "Downloading it…" bash -c 'echo noisy; exit 0' 2>&1)
check "quietly: progress" "$out" "^→ Downloading it…$" has
check "quietly: done, as done" "$out" "^✓ Downloaded it$" has
check "quietly: no output when it works" "$out" "noisy" lacks
# Off a terminal, two steps with the same line show it once.
out=$( (quietly "Fetching…" true; quietly "Fetching…" true) 2>&1)
check "quietly: a repeated line once" "$(grep -c '^→ Fetching…$' <<<"$out")" "^1$" has
out=$(quietly "Doing it…" bash -c 'echo what went wrong; exit 3' 2>&1) && rc=0 || rc=$?
check "quietly: the output when it fails" "$out" "what went wrong" has
check "quietly: the step failed" "$out" "^✗ Doing it$" has
check "quietly: and its exit code" "$rc" "^3$" has
# A failure ends the progress line first.
out=$( (progress_start "Working…"; fail "Broke.") 2>&1 || true)
check "fail after progress" "$out" "Error: Broke\." has
check "fail: the step failed first" "$out" $'^✗ Working\nError: Broke\.$' has

# Off a terminal: no colors, nothing drawn over.
out=$( (say "Installing Pyo 9.9.9"; ok_line "Pyo 9.9.9 is installed"; dot_line green "Running"; quietly "Starting k3s…" true) 2>&1)
check "no terminal: no escapes" "$out" $'\033|\r' lacks
check "no terminal: a heading" "$out" "^→ Installing Pyo 9\.9\.9$" has
check "no terminal: the last line" "$out" "^✓ Pyo 9\.9\.9 is installed$" has
check "no terminal: a status" "$out" "^● Running$" has
check "no terminal: a step done" "$out" "^✓ Started k3s$" has
# A step's time, once it took a while.
sleep() { SECONDS=$((SECONDS + ${1%.*})); }
out=$( (progress_start "Downloading k3s…"; sleep 12; progress_end) 2>&1)
check "a long step: its time" "$out" "^✓ Downloaded k3s 0:12$" has
out=$( (progress_start "Starting Pyo 9.9.9 (a few minutes the first time)…"; progress_end ok "Started Pyo 9.9.9") 2>&1)
check "a step's own words when done" "$out" "^✓ Started Pyo 9\.9\.9$" has
unset -f sleep
# On a terminal (script(1) gives one): the step ticks on its line, then
# stays as a checklist, in color. NO_COLOR: the same, without colors.
if command -v script >/dev/null; then
  blk=$(mktemp)
  sed -n '/^# >>> pyo:progress/,/^# <<< pyo:progress/p' deploy/install.sh >"$blk"
  tty_run() { script -qec "bash -c '. $blk; $1'" /dev/null 2>/dev/null | tr -d '\r' || true; }
  out=$(tty_run 'say Installing; quietly "Downloading k3s…" sleep 1; quietly "Checking it…" false; ok_line Done; dot_line yellow Starting')
  check "terminal: a bold heading" "$out" $'\033\\[1m→ Installing\033\\[0m' has
  check "terminal: a ticking step in cyan" "$out" $'\033\\[36m→\033\\[0m Downloading k3s…' has
  check "terminal: a green check" "$out" $'\033\\[32m✓\033\\[0m Downloaded k3s' has
  check "terminal: a red cross" "$out" $'\033\\[31m✗ Checking it\033\\[0m' has
  check "terminal: the last line in green" "$out" $'\033\\[1;32m✓ Done\033\\[0m' has
  check "terminal: a yellow dot" "$out" $'\033\\[33m●\033\\[0m Starting' has
  out=$(NO_COLOR=1 tty_run 'say Installing; quietly "Downloading k3s…" true')
  check "NO_COLOR: no colors on a terminal" "$out" $'\033\\[[0-9;]*m' lacks
  check "NO_COLOR: the checklist still" "$out" "✓ Downloaded k3s" has
  rm -f "$blk"
fi

[ "$failed" = 0 ] && echo "install.sh: all checks passed"
exit "$failed"
