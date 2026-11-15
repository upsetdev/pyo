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

# The pyo-admin Secret is there, as it stays after the first sign-in.
kc() { return 0; }

ADMIN_PASSWORD=s3cret
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
out=$(count_install)
check "telemetry off says both are off" "$out" "Not counting this install, and usage counts are off" has
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
unset -f curl sleep

[ "$failed" = 0 ] && echo "install.sh: all checks passed"
exit "$failed"
