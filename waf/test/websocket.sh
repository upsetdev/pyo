#!/usr/bin/env bash
# websocket.sh WAF [OLD_WAF]: WebSockets through pyo-waf and Envoy v1.39.5
# (the one Envoy Gateway v1.9.3 runs), in docker. WAF: a pyo-waf binary
# (./build.sh writes bin/pyo-waf). OLD_WAF: optional, an older one, to
# check it with Pyo's new policy.
#
# Each listener is one ext_proc setting (ws-envoy.py): block mode and log
# only, each with and without allowModeOverride. Checks that a WebSocket
# opens and echoes, that its handshake gets the WAF's rules, that one
# with a body is refused, what a request pipelined behind a handshake
# does, and that an open WebSocket outlives a WAF restart. Prints ok or
# FAIL per check, and exits 1 on any FAIL.
#
# Throwaway containers and network named zd-ws-*, removed on exit. Images
# pinned by digest.
set -uo pipefail
WAF=$(realpath "${1:?usage: websocket.sh WAF [OLD_WAF]}")
OLD=${2:+$(realpath "$2")}
ENVOY=docker.io/envoyproxy/envoy:distroless-v1.39.5@sha256:9d84a586119a27b50437dbb01eccd4722832741539b1ed53c8e9d6da6067a6e0
BACKEND=docker.io/traefik/whoami:v1.11.0@sha256:200689790a0a0ea48ca45992e0450bc26ccab5307375b41c84dfc4f2475937ab
HERE=$(cd "$(dirname "$0")" && pwd)
PROBE="python3 -I $HERE/ws-probe.py"
FAILED=0

stop() {
  docker rm -f zd-ws-envoy zd-ws-waf zd-ws-waf-old zd-ws-backend >/dev/null 2>&1
  docker network rm zd-ws-net >/dev/null 2>&1
}
stop
DIR=$(mktemp -d)
trap 'stop; rm -rf "$DIR"' EXIT

ok() { printf '  ok    %s\n' "$*"; }
bad() { printf '  FAIL  %s\n' "$*"; FAILED=1; }
# check NAME WANT GOT: GOT contains WANT. same: GOT is WANT.
check() { case "$3" in *"$2"*) ok "$1: $3" ;; *) bad "$1: $3 (want $2)" ;; esac; }
same() { if [ "$3" = "$2" ]; then ok "$1: $3"; else bad "$1: $3 (want $2)"; fi; }
code() { curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$@"; }
run() { $PROBE "$@" | tr '\n' ' '; }
xss='%3Cscript%3Ealert(1)%3C/script%3E'

python3 -I "$HERE/ws-envoy.py" "$DIR/envoy.json" "$DIR/waf.json"
chmod 755 "$DIR"
chmod 644 "$DIR"/*.json
echo "Starting the containers"
docker network create zd-ws-net >/dev/null
docker run -d --name zd-ws-backend --network zd-ws-net --memory 256m "$BACKEND" -verbose >/dev/null
waf() {
  docker run -d --name "$1" --network zd-ws-net --memory 1g -u "$(id -u):$(id -g)" \
    -v "$2:/pyo-waf:ro" -v "$DIR/waf.json:/etc/waf.json:ro" --entrypoint /pyo-waf \
    "$ENVOY" -listen :9002 -config /etc/waf.json >/dev/null
}
waf zd-ws-waf "$WAF"
[ -n "$OLD" ] && waf zd-ws-waf-old "$OLD"
docker run -d --name zd-ws-envoy --network zd-ws-net --memory 1g -v "$DIR/envoy.json:/etc/envoy.json:ro" \
  -p 127.0.0.1:19901:9901 $(for p in 10001 10002 10003 10004 10005 10006; do echo -p "127.0.0.1:$p:$p"; done) \
  "$ENVOY" -c /etc/envoy.json --drain-time-s 6 >/dev/null
echo "Waiting for the WAF's rules"
for p in 10001 ${OLD:+10005}; do
  timeout 180 bash -c "until [ \"\$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: block.test' 'http://127.0.0.1:$p/?q=$xss')\" = 403 ]; do sleep 2; done" ||
    { bad "the WAF on $p never blocked"; docker logs zd-ws-waf 2>&1 | tail -5; exit 1; }
done

echo "Requests without an upgrade"
check "block, GET" 200 "$(code -H 'Host: block.test' http://127.0.0.1:10001/)"
check "block, XSS in the query" 403 "$(code -H 'Host: block.test' "http://127.0.0.1:10001/?q=$xss")"
check "block, XSS in a form" 403 "$(code -H 'Host: block.test' -d 'q=<script>alert(1)</script>' http://127.0.0.1:10001/)"
check "block, clean form" 200 "$(code -H 'Host: block.test' -d 'a=b' http://127.0.0.1:10001/)"
check "log only, XSS in a form" 200 "$(code -H 'Host: log.test' -d 'q=<script>alert(1)</script>' http://127.0.0.1:10002/)"

echo "WebSocket open and echo (the frame carries <script>)"
check "block" "101 Switching Protocols" "$(run 10001 block.test /echo)"
check "block, echo" "echo 0: ok" "$(run 10001 block.test /echo)"
check "log only" "echo 0: ok" "$(run 10002 log.test /echo)"
check "log only, XSS in the query (logged)" "echo 0: ok" "$(run 10002 log.test "/echo?q=$xss")"

echo "The handshake gets the WAF's rules (block)"
check "XSS in the query" "403 Forbidden" "$(run 10001 block.test "/echo?q=$xss")"
check "SQL injection in the query" "403 Forbidden" "$(run 10001 block.test "/echo?id=1%27%20OR%20%271%27%3D%271")"
check "XSS in a header" "403 Forbidden" "$(run 10001 block.test /echo 'Referer: http://example.test/<script>alert(1)</script>')"
check "scanner user agent" "403 Forbidden" "$(run 10001 block.test /echo 'User-Agent: sqlmap/1.7')"
check "path traversal" "403 Forbidden" "$(run 10001 block.test '/echo/../../etc/passwd')"
check "with a body (Content-Length)" "403 Forbidden" "$(run 10001 block.test /echo 'BODY=a=b')"
# Envoy doesn't hand the WAF Transfer-Encoding: the chunked body goes on
# as the WebSocket's data (so the echo is wrong), uninspected like its
# messages.
check "with a body (chunked), passes as data" "101 Switching Protocols" "$(run 10001 block.test /echo 'CHUNKED=a=b')"
check "log only, with a body" "403 Forbidden" "$(run 10002 log.test /echo 'BODY=a=b')"

echo "Older policy (no allowModeOverride): waits for a body, as before"
check "block" TIMEOUT "$(run 10003 block.test /echo)"
check "log only" TIMEOUT "$(run 10004 log.test /echo)"
check "block, XSS in the query" "403 Forbidden" "$(run 10003 block.test "/echo?q=$xss")"
if [ -n "$OLD" ]; then
  echo "Older WAF, new policy: as before"
  check "block" TIMEOUT "$(run 10005 block.test /echo)"
  check "log only" TIMEOUT "$(run 10006 log.test /echo)"
fi

echo "Frames don't go to the WAF"
msgs() { curl -s 127.0.0.1:19901/stats | awk -v k="http.l$1.ext_proc.stream_msgs_sent:" '$1 == k { print $2 }'; }
before=$(msgs 10001)
N=5 $PROBE 10001 block.test /echo >/dev/null
sleep 1
same "5 frames, messages to the WAF" 1 "$(($(msgs 10001) - before))"

echo "A request pipelined behind a handshake"
for c in "10001 block.test /" "10001 block.test /echo" "10002 log.test /" "10003 block.test /"; do
  set -- $c
  echo "  $1 $2 $3: $(python3 -I "$HERE/ws-pipeline.py" "$1" "$2" "$3")"
done
sleep 1
check "the backend logs requests" "/echo" "$(docker logs zd-ws-backend 2>&1 | grep -m1 -o '/echo')"
same "pipelined requests the backend saw" 0 "$(docker logs zd-ws-backend 2>&1 | grep -c '/pipelined')"

echo "An open WebSocket outlives a WAF restart (a frame every 2 s for 14 s)"
for c in "10001 block.test" "10002 log.test"; do
  set -- $c
  (sleep 3; docker restart -t 5 zd-ws-waf >/dev/null) &
  out=$(N=7 HOLD=2 $PROBE "$1" "$2" /echo | tr '\n' ' ')
  wait
  check "$1 $2" "echo 6: ok" "$out"
  timeout 120 bash -c "until [ \"\$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: block.test' 'http://127.0.0.1:10001/?q=$xss')\" = 403 ]; do sleep 2; done"
done

echo
echo "WAF log lines:"
docker logs zd-ws-waf 2>/dev/null | grep '"waf":"pyo"' | cut -c1-200 | tail -12
[ "$FAILED" = 0 ] && echo "All checks passed." || echo "Some checks failed."
exit "$FAILED"
