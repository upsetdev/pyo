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

[ "$failed" = 0 ] && echo "install.sh: all checks passed"
exit "$failed"
