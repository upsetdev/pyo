#!/usr/bin/env bash
# Made by Pyo: joins this server to Pyo over your tailnet.
#
#   boot.sh PYO SERVER CLUSTER ROLE NAME
#
# 1. Joins your tailnet with a single-use Tailscale key.
# 2. Trades a single-use code for this server's join details at Pyo, over
#    the tailnet (PYO: Pyo's tailnet door).
# 3. Runs join.sh, connect.sh for a new cluster, or sandbox.sh for a
#    sandbox server (ROLE), with them. For a sandbox server CLUSTER is the
#    sandbox server's ID.
#
# The key and the code are the only secrets in this server's user data.
# They're in /run/pyo-boot (root only, in memory) and deleted once used.
# The join details stay in this shell: never in a file, nor on a command
# line. The log is /var/log/pyo-join.log.
exec >>/var/log/pyo-join.log 2>&1
set -u
PYO=$1 SERVER=$2 CLUSTER=$3 ROLE=$4 NAME=$5
DIR=/run/pyo-boot
trap 'rm -f "$DIR/tailscale-key" "$DIR/code"' EXIT
say() { echo "$(date -u +%FT%TZ) $*"; }
fail() {
  say "Error: $*"
  exit 1
}

say "Installing Tailscale"
if ! command -v tailscale >/dev/null; then
  curl -fsSL --retry 5 https://tailscale.com/install.sh | sh || fail "Tailscale didn't install."
fi
systemctl enable --now tailscaled || fail "Tailscale didn't start."

say "Joining your tailnet as $NAME"
tailscale up --auth-key="file:$DIR/tailscale-key" --hostname="$NAME" --timeout=120s || true
rm -f "$DIR/tailscale-key"
[ -n "$(tailscale ip -4 2>/dev/null)" ] || fail "This server didn't join your tailnet."

# Until Pyo answers: with Tailnet Lock on, nothing does until this server's
# key is signed (Pyo shows the command). The code expires 15 minutes after
# the server was made.
say "Asking Pyo for this server's join details"
deadline=$((SECONDS + 900))
while :; do
  out=$(curl -sS -m 30 -w '\n%{http_code}' -X POST -H 'Content-Type: text/plain' \
    --data-binary "@$DIR/code" "$PYO/cloud/bootstrap?server=$SERVER&cluster=$CLUSTER&role=$ROLE")
  status=${out##*$'\n'} body=${out%$'\n'*}
  case "$status" in
    200) break ;;
    000 | 429 | 5??) ;;
    *) fail "Pyo refused: $body" ;;
  esac
  [ "$SECONDS" -lt "$deadline" ] || fail "Pyo didn't answer over the tailnet in 15 minutes."
  sleep 5
done
rm -f "$DIR/code"

# >>> parse
script="" args=()
while IFS= read -r line; do
  case "$line" in
    script=*) script=${line#script=} ;;
    arg=*) args+=("${line#arg=}") ;;
  esac
done < <(printf '%s\n' "$body")
unset out body
# <<< parse
case "$ROLE:$script" in
  join:http*/join.sh | connect:http*/connect.sh | sandbox:http*/sandbox.sh) ;;
  *) fail "Pyo's answer has no $ROLE script." ;;
esac

say "Fetching $script"
curl -fsSL --retry 5 -o "$DIR/run.sh" "$script" || fail "Couldn't fetch $script."
say "Running ${script##*/}"
set -- "${args[@]}"
unset args
# shellcheck disable=SC1091
. "$DIR/run.sh"
