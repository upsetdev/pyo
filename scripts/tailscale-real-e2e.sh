#!/usr/bin/env bash
# Tailscale for all clusters against a real, throwaway tailnet
# (docs/tailscale-all-clusters.md, "Real tailnet, once"). Never the user's
# own tailnet, servers or VMs: it makes its own multipass VMs and deletes
# them at the end.
#
#   PYO_URL      the test Pyo, built from this branch, reachable from the VMs
#   PYO_SESSION  a dashboard session of a team admin there (the pyo_session
#                cookie), for the dashboard-only Tailscale ops
#   OP_ITEM      the 1Password item with the throwaway tailnet's API access
#                token (default "Tailscale pyo test"), its field OP_FIELD
#                (default "credential"). Read by `op` and piped straight to
#                Pyo: never printed, never in argv. Pyo deletes the token
#                after a setup, so step 4 asks for a fresh one in the same
#                item.
#   TEAM         the test team's id (a new, empty team)
#
#   scripts/tailscale-real-e2e.sh vms        two VMs (a third comes in step 2)
#   scripts/tailscale-real-e2e.sh test       steps 1 to 6
#   scripts/tailscale-real-e2e.sh clean      step 7: the VMs go
#
# Each step says what it checks. A step that needs the dashboard says so
# and waits for Enter. Needs multipass, jq, curl and op. At least 10 GB
# available: each VM takes 2.5 GB.
set -euo pipefail
VMS=(ts-e2e-a ts-e2e-b ts-e2e-c)
on() { local vm=$1; shift; multipass exec "$vm" -- sudo bash -c "$*"; }
say() { printf '\n== %s\n' "$*"; }
pass() { printf '  PASS  %s\n' "$*"; }
fail() { printf '  FAIL  %s\n' "$*"; FAILED=$((FAILED + 1)); }
check() { local what=$1; shift; if "$@"; then pass "$what"; else fail "$what"; fi; }
wait_enter() { read -r -p "  $* Then press Enter. " _; }
FAILED=0
OP_ITEM=${OP_ITEM:-Tailscale pyo test}
OP_FIELD=${OP_FIELD:-credential}

# The token, from 1Password, onto stdout for the next pipe only.
token() { op item get "$OP_ITEM" --fields "label=$OP_FIELD" --reveal; }
# planTailscaleSetup, then setUpTailscale as reviewed: the setup's outcome.
set_up_team() {
  local plan version
  plan=$(token | jq -Rsc --arg team "$TEAM" '{op: "planTailscaleSetup", teamId: $team, token: (. | rtrimstr("\n"))}' | op_call)
  version=$(jq -r .plan.version <<<"$plan")
  echo "  It runs on $(jq -r .clusterId <<<"$plan"). Pyo will: $(jq -r '.plan.changes | join(", ")' <<<"$plan")" >&2
  token | jq -Rsc --arg team "$TEAM" --arg v "$version" \
    '{op: "setUpTailscale", teamId: $team, token: (. | rtrimstr("\n")), version: $v}' | op_call
}

need() { [ -n "${!1:-}" ] || { echo "Set $1." >&2; exit 2; }; }

# A dashboard op, as the dashboard sends it.
op_call() {
  curl -fsS -X POST "$PYO_URL/api/ops" -H "content-type: application/json" -H "x-pyo-csrf: 1" \
    -H "cookie: pyo_session=$PYO_SESSION" --data-binary @-
}
state() { curl -fsS "$PYO_URL/api/state" -H "cookie: pyo_session=$PYO_SESSION"; }
team_ts() { state | jq ".teams[] | select(.id == \"$TEAM\") | .tailscale"; }
cluster_id() { state | jq -r ".clusters[] | select(.teamId == \"$TEAM\" and .name == \"$1\") | .id"; }
# The tailnet a VM is on, and the machines it sees there.
suffix_of() { on "$1" "tailscale status --json 2>/dev/null | jq -r .MagicDNSSuffix" | tr -d '\r'; }
peers_of() { on "$1" "tailscale status --json 2>/dev/null | jq -r '.Peer[]?.HostName'" | tr -d '\r'; }
until_ok() { local n=$1; shift; for _ in $(seq "$n"); do "$@" && return 0; sleep 10; done; return 1; }

vm() {
  multipass info "$1" >/dev/null 2>&1 || multipass launch 24.04 --name "$1" --cpus 2 --memory 2500M --disk 15G
}

# A new single-server cluster on a VM: Add cluster in the dashboard, its
# command run there. No Tailscale paste for it.
connect() {
  local vm=$1
  vm "$vm"
  say "Connect $vm as a cluster named $vm"
  echo "  In the test Pyo: Clusters › Add cluster, name it $vm, and copy its command."
  read -r -p "  Paste the command: " cmd
  on "$vm" "$cmd"
  check "$vm is connected" until_ok 30 bash -c "[ \"\$(state_status $vm)\" = connected ]"
}
state_status() { state | jq -r ".clusters[] | select(.teamId == \"$TEAM\" and .name == \"$1\") | .status"; }
# The checks run these in `bash -c`.
export -f on state state_status team_ts suffix_of peers_of until_ok
export TEAM PYO_URL PYO_SESSION

case "${1:-}" in
vms)
  avail=$(free -g | awk '/^Mem:/ { print $7 }')
  [ "$avail" -ge 10 ] || { echo "Only ${avail} GB available. Needs 10." >&2; exit 1; }
  vm ts-e2e-a
  vm ts-e2e-b
  ;;
test)
  for v in PYO_URL PYO_SESSION TEAM; do need "$v"; done

  connect ts-e2e-a
  connect ts-e2e-b

  say "1. Set up Tailscale for the team, once"
  set_up_team | jq '{ok, changed, todo, error, tokenDeleted}'
  check "the team has a holder" bash -c "[ \"\$(team_ts | jq -r .holderId)\" != null ]"

  say "2. A new cluster joins with no paste"
  connect ts-e2e-c
  holder_name=$(team_ts | jq -r .holderName)
  echo "  In the dashboard: Clusters › ts-e2e-c › Add server shows a join command with no Tailscale step."
  echo "  And Integrations › Tailscale lists ts-e2e-a, ts-e2e-b and ts-e2e-c, kept on $holder_name."
  wait_enter "Check both."
  suffix=$(team_ts | jq -r .suffix)
  for v in ts-e2e-a ts-e2e-b ts-e2e-c; do
    check "$v is on $suffix" until_ok 30 bash -c "[ \"\$(suffix_of $v)\" = '$suffix' ]"
  done
  check "one Pyo OAuth client on the team" bash -c "[ \"\$(team_ts | jq '.oldClients | length')\" = 0 ]"
  echo "  In the throwaway tailnet's console, Settings › Trust credentials shows one Pyo client."
  wait_enter "Check it."

  say "3. Tailnet names on two clusters"
  echo "  Deploy a small service on ts-e2e-a and on ts-e2e-b, each with a tailnet domain"
  echo "  (blog-a.$suffix and blog-b.$suffix)."
  wait_enter "Deploy both."
  for h in blog-a blog-b; do
    check "$h.$suffix answers over HTTPS from the tailnet" until_ok 60 \
      on ts-e2e-c "curl -fsS -m 10 -o /dev/null https://$h.$suffix/"
  done

  say "4. The holder offline, then back"
  holder=$holder_name
  multipass stop "$holder"
  check "the team says who it waits for" until_ok 60 bash -c "[ \"\$(team_ts | jq -r .holderConnected)\" = false ]"
  for h in blog-a blog-b; do
    check "$h still answers" on ts-e2e-c "curl -fsS -m 10 -o /dev/null https://$h.$suffix/" || true
  done
  echo "  Add server on another cluster now says $holder keeps your Tailscale and isn't connected."
  wait_enter "Check it."

  say "4b. A new holder while it's away, and its old client deleted"
  old_client=$(team_ts | jq -r .clientId)
  echo "  Pyo deleted the first token. Make a new API access token in the throwaway console"
  echo "  and put it in the same 1Password item, in place of the first."
  wait_enter "Update the item."
  OUTCOME=$(set_up_team)
  export OUTCOME
  jq '{ok, changed, todo, error, tokenDeleted, oldClients}' <<<"$OUTCOME"
  check "the setup went through" bash -c '[ "$(jq -r .ok <<<"$OUTCOME")" = true ]'
  check "another cluster keeps it now" bash -c "[ \"\$(team_ts | jq -r .holderName)\" != '$holder' ]"
  check "the old client $old_client was deleted" \
    bash -c "jq -e '.oldClients[]? | select(.clientId == \"$old_client\" and .state == \"deleted\")' <<<\"\$OUTCOME\" >/dev/null"
  check "it's off the console list" bash -c "! team_ts | jq -e '.oldClients[] | select(.clientId == \"$old_client\")' >/dev/null"
  echo "  In the throwaway console, Settings › Trust credentials no longer shows $old_client."
  wait_enter "Check it."

  multipass start "$holder"
  check "$holder is back as a member" until_ok 60 bash -c \
    "[ \"\$(team_ts | jq -r '.clusters[] | select(.state == \"member\") | .id' | grep -c .)\" -ge 1 ]"
  holder=$(team_ts | jq -r .holderName)

  say "5. A removed cluster's server leaves the tailnet"
  gone=ts-e2e-c
  [ "$gone" != "$holder" ] || gone=ts-e2e-b
  other=ts-e2e-a; [ "$other" != "$gone" ] || other=ts-e2e-b
  check "$gone is a peer before" bash -c "peers_of $other | grep -qx $gone"
  echo "  Delete its services, then Clusters › $gone › Settings › Remove from Pyo."
  wait_enter "Remove it."
  check "$gone left the tailnet" until_ok 60 bash -c "! peers_of $other | grep -qx $gone"

  say "6. Disconnect"
  client=$(team_ts | jq -r .clientId)
  jq -nc --arg team "$TEAM" '{op: "removeTailscale", teamId: $team}' | op_call >/dev/null
  check "the team has no Tailscale" bash -c "[ \"\$(team_ts)\" = null ]"
  check "the tailnet names are gone" until_ok 30 bash -c "! on $other 'curl -fsS -m 10 -o /dev/null https://blog-a.$suffix/'"
  echo "  Servers stay on the tailnet. Delete OAuth client $client in the throwaway console."
  echo
  [ "$FAILED" = 0 ] && echo "All passed." || { echo "$FAILED failed."; exit 1; }
  ;;
clean)
  say "7. Clean up"
  for v in "${VMS[@]}"; do multipass delete --purge "$v" 2>/dev/null || true; done
  echo "  The VMs are gone. Delete the throwaway tailnet in its console."
  ;;
*)
  sed -n '2,22p' "$0"
  exit 2
  ;;
esac
