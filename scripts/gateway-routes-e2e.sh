#!/usr/bin/env bash
# Checks Pyo Gateway's rules on a real install, end to end: redirects,
# header rules and routes (paths of one hostname on two services), as a
# visitor sees them (docs/gateway-routes.md).
#
# It deploys a throwaway project of two small echo services with free
# nip.io domains, sets rules with the pyo CLI, checks them with curl, and
# deletes the project. Run it on a throwaway install, never on one people
# use: it needs a cluster that takes public traffic and whose free domains
# are on its public IP (Let's Encrypt has to reach it).
#
#   PYO_URL=https://pyo.example.test PYO_API_KEY=pyo_… ./scripts/gateway-routes-e2e.sh
#
# Optional: PYO_BIN (the pyo CLI, default "pyo"), ECHO_IMAGE (an HTTP echo
# server on port 80 that answers with the request it got), PYO_TEAM and
# PYO_CLUSTER (where the project goes). Needs curl and jq.
set -euo pipefail

: "${PYO_URL:?Set PYO_URL to the throwaway install.}"
: "${PYO_API_KEY:?Set PYO_API_KEY to an API key with full access.}"
PYO="${PYO_BIN:-pyo}"
ECHO_IMAGE="${ECHO_IMAGE:-docker.io/traefik/whoami:v1.11.0}"
RUN="e2e-routes-$(date +%s | tail -c 6)$((RANDOM % 90 + 10))"
DIR="$(mktemp -d)"
PROJECT_ID=""
FAILED=0

say() { printf '%s\n' "$*"; }
ok() { printf '  ok    %s\n' "$*"; }
bad() {
  printf '  FAIL  %s\n' "$*"
  FAILED=1
}

op() {
  curl -fsS -H "Authorization: Bearer $PYO_API_KEY" -H 'Content-Type: application/json' \
    -d "$1" "$PYO_URL/api/v1/ops"
}
state() { curl -fsS -H "Authorization: Bearer $PYO_API_KEY" "$PYO_URL/api/v1/state"; }

cleanup() {
  if [ -n "$PROJECT_ID" ]; then
    say "Deleting $RUN"
    op "{\"op\":\"deleteProject\",\"id\":\"$PROJECT_ID\"}" >/dev/null || say "Couldn't delete $RUN. Delete it in the dashboard."
  fi
  rm -rf "$DIR"
}
trap cleanup EXIT

# One request: status, then headers, then the body.
fetch() { curl -ksS -D - -o "$DIR/body" --max-time 15 "$@"; }
header_of() { grep -i "^$1:" | head -1 | cut -d' ' -f2- | tr -d '\r'; }
status_of() { head -1 | cut -d' ' -f2; }

say "Deploying $RUN"
cat >"$DIR/compose.yml" <<EOF
services:
  web:
    image: $ECHO_IMAGE
    ports: ["80"]
  api:
    image: $ECHO_IMAGE
    ports: ["80"]
EOF
(cd "$DIR" && "$PYO" deploy -f compose.yml --project "$RUN" --yes \
  ${PYO_TEAM:+--team "$PYO_TEAM"} ${PYO_CLUSTER:+--cluster "$PYO_CLUSTER"} >/dev/null)
PROJECT_ID=$(state | jq -r --arg n "$RUN" '.projects[] | select(.name == $n) | .id')
[ -n "$PROJECT_ID" ] || { say "No project $RUN after the deploy."; exit 1; }

host_of() {
  state | jq -r --arg p "$PROJECT_ID" --arg s "$1" \
    '.services[] | select(.projectId == $p and .name == $s) | .domains[0].hostname // empty'
}
# Its free domain: the one the import suggested, else a new one.
live() {
  for _ in $(seq 120); do
    [ "$(curl -ksS -o /dev/null -w '%{http_code}' --max-time 5 "https://$1/" || true)" = 200 ] && return 0
    sleep 5
  done
  say "$1 didn't answer over HTTPS in 10 minutes."
  exit 1
}
say "Waiting for the free domains and their certificates"
for s in web api; do
  [ -n "$(host_of $s)" ] || "$PYO" domain add --free -s "$RUN/$s" >/dev/null
  live "$(host_of $s)"
done
WEB=$(host_of web)
# A second name at the same address, for a domain that only redirects.
WWW="www-${WEB}"
say "  web: $WEB"

say "Redirects"
"$PYO" domain add "$WWW" --redirect-to "$WEB" --wait -s "$RUN/web" >/dev/null
out=$(fetch "https://$WWW/some/page?q=1")
[ "$(status_of <<<"$out")" = 301 ] && ok "www redirects with 301" || bad "www: $(status_of <<<"$out")"
loc=$(header_of location <<<"$out")
[ "$loc" = "https://$WEB/some/page?q=1" ] && ok "path and query kept" || bad "www went to $loc"
out=$(curl -sS -D - -o /dev/null --max-time 15 "http://$WWW/x")
loc=$(header_of location <<<"$out")
[ "$loc" = "https://$WEB/x" ] && ok "plain HTTP goes straight to the target" || bad "plain HTTP went to $loc"

"$PYO" domain redirect "$WEB/old" --to /new --status 302 -s "$RUN/web" >/dev/null
sleep 5
out=$(fetch "https://$WEB/old/a?b=c")
[ "$(status_of <<<"$out")" = 302 ] && ok "path redirect with 302" || bad "/old: $(status_of <<<"$out")"
loc=$(header_of location <<<"$out")
[ "$loc" = "https://$WEB/new/a?b=c" ] && ok "rest of the path kept" || bad "/old went to $loc"
out=$(fetch "https://$WEB/older")
[ "$(status_of <<<"$out")" = 200 ] && ok "/older isn't under /old" || bad "/older: $(status_of <<<"$out")"
"$PYO" domain redirect "$WEB/old" --remove -s "$RUN/web" >/dev/null

say "Headers"
"$PYO" domain header "$WEB" --set 'X-Frame-Options: DENY' --set 'X-E2e: yes' -s "$RUN/web" >/dev/null
"$PYO" domain header "$WEB" --request --set 'X-From-Gateway: 1' -s "$RUN/web" >/dev/null
sleep 5
out=$(fetch "https://$WEB/")
[ "$(header_of x-frame-options <<<"$out")" = DENY ] && ok "response header set" || bad "no X-Frame-Options"
[ "$(header_of x-e2e <<<"$out")" = yes ] && ok "a second response header" || bad "no X-E2e"
grep -qi '^X-From-Gateway: 1' "$DIR/body" && ok "request header reaches the service" || bad "the service didn't get X-From-Gateway"
if "$PYO" domain header "$WEB" --request --set 'X-Forwarded-For: 1.2.3.4' -s "$RUN/web" >/dev/null 2>&1; then
  bad "X-Forwarded-For could be set"
else
  ok "Pyo Gateway's own headers can't be changed"
fi

say "Routes"
"$PYO" domain add "$WEB/api" --strip-prefix -s "$RUN/api" >/dev/null
sleep 5
fetch "https://$WEB/api/users?x=1" >/dev/null
grep -qi '^GET /users?x=1 ' "$DIR/body" && ok "/api goes to api, prefix stripped" || bad "api didn't get /users: $(head -1 "$DIR/body")"
api_name=$(grep -i '^Hostname:' "$DIR/body" | cut -d' ' -f2 | tr -d '\r')
fetch "https://$WEB/apix" >/dev/null
web_name=$(grep -i '^Hostname:' "$DIR/body" | cut -d' ' -f2 | tr -d '\r')
[ -n "$api_name" ] && [ "$api_name" != "$web_name" ] && ok "/apix stays on web" || bad "/apix went to the same service as /api"
grep -qi '^GET /apix ' "$DIR/body" && ok "web gets its path as it was" || bad "web got $(head -1 "$DIR/body")"
# web's header rules apply to the whole hostname, api's paths included.
out=$(fetch "https://$WEB/api/x")
[ "$(header_of x-frame-options <<<"$out")" = DENY ] && ok "the hostname's header rules on a path" || bad "no X-Frame-Options on /api"
# web stops serving the hostname: api takes it over, and it keeps answering.
"$PYO" domain remove "$WEB" --yes --keep-dns-record -s "$RUN/web" >/dev/null
sleep 10
fetch "https://$WEB/api/users" >/dev/null
grep -qi '^GET /users ' "$DIR/body" && ok "api keeps the hostname when web leaves it" || bad "after web left: $(head -1 "$DIR/body")"

if [ "$FAILED" = 0 ]; then
  say "All checks passed."
else
  say "Some checks failed."
  exit 1
fi
