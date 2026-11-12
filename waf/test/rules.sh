#!/usr/bin/env bash
# rules.sh CONFIG GEOIP [PORT]: custom rules end to end. Runs bin/pyo-waf with
# CONFIG (pyo-api's waf_harness_config test writes it) and the GeoIP country
# database GEOIP, behind Envoy on 127.0.0.1:PORT (default 19185). Envoy
# trusts one X-Forwarded-For hop, as a cluster behind a CDN does, so the
# probes can pick their address. Prints each probe and whether it passed.
# Stop with: docker rm -f waf-rules-proc waf-rules-envoy; docker network rm waf-rules
set -euo pipefail
CONFIG=$(realpath "$1"); GEOIP=$(realpath "$2"); P=${3:-19185}
HERE=$(cd "$(dirname "$0")" && pwd); BIN=$HERE/../bin/pyo-waf
DIR=$(mktemp -d)
docker rm -f waf-rules-proc waf-rules-envoy >/dev/null 2>&1 || true
docker network create waf-rules >/dev/null 2>&1 || true
python3 "$HERE/envoy.py" "$DIR" unused
python3 - "$DIR/extproc.json" <<'PY'
import json, sys
p = sys.argv[1]; c = json.load(open(p))
hcm = c["static_resources"]["listeners"][0]["filter_chains"][0]["filters"][0]["typed_config"]
hcm["use_remote_address"] = True
hcm["xff_num_trusted_hops"] = 1
for cl in c["static_resources"]["clusters"]:
    for e in cl["load_assignment"]["endpoints"][0]["lb_endpoints"]:
        e["endpoint"]["address"]["socket_address"]["address"] = "waf-rules-proc"
json.dump(c, open(p, "w"), indent=1)
PY
chmod 755 "$DIR"; chmod a+r "$DIR"/*
docker run -d --name waf-rules-proc --network waf-rules -v "$BIN:/pyo-waf:ro" -v "$CONFIG:/etc/pyo-waf/config.json:ro" -v "$GEOIP:/geoip/c.mmdb:ro" \
  gcr.io/distroless/static-debian12:nonroot /pyo-waf -listen :9002 -config /etc/pyo-waf/config.json -geoip /geoip/c.mmdb >/dev/null
docker run -d --name waf-rules-envoy --network waf-rules -p "127.0.0.1:$P:10000" -v "$DIR:/waf:ro" envoyproxy/envoy:v1.39.5 -c /waf/extproc.json >/dev/null
timeout 120 bash -c "until curl -s -o /dev/null http://127.0.0.1:$P/; do sleep 1; done"
sleep 2

fails=0
probe() { # probe WANT LABEL CURL-ARGS...
  local want=$1 label=$2; shift 2
  local got; got=$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: rules.test' "$@")
  if [ "$got" = "$want" ]; then echo "ok   $got  $label"; else echo "FAIL $got (want $want)  $label"; fails=$((fails + 1)); fi
}
U=http://127.0.0.1:$P
SQLI='?id=1%27%20OR%20%271%27=%271'
probe 200 "a normal request"                                   "$U/"
probe 403 "Block rule on a path: /wp-admin"                    "$U/wp-admin/login.php"
probe 403 "Block rule on the decoded path: /%77p-admin"       "$U/%77p-admin"
probe 403 "OWASP rules block SQL injection"                    "$U/$SQLI"
probe 200 "Allow rule for an IP skips the OWASP rules"         -H 'X-Forwarded-For: 198.51.100.7' "$U/$SQLI"
probe 200 "Skip OWASP: SQL injection on /skip goes through"    "$U/skip$SQLI"
probe 403 "Skip OWASP: later custom rules still run"           -A badbot "$U/skip$SQLI"
probe 403 "AND rule: POST and /and"                            -X POST "$U/and"
probe 200 "AND rule: GET /and (one condition only)"            "$U/and"
probe 200 "AND rule: POST /other (one condition only)"         -X POST "$U/other"
probe 403 "OR rule: the header"                                -H 'X-Bad: 1' "$U/"
probe 403 "OR rule: the user agent"                            -A 'Mozilla badbot/1' "$U/"
probe 200 "OR rule: neither"                                   -H 'X-Bad: 0' "$U/"
probe 403 "Country rule: US address on /geo"                   -H 'X-Forwarded-For: 8.8.8.8' "$U/geo"
probe 200 "Country rule: German address on /geo"               -H 'X-Forwarded-For: 85.214.132.117' "$U/geo"
probe 200 "Allow rule on a path skips later rules and OWASP"   "$U/order$SQLI"
probe 200 "Log rule: logged, goes through"                     "$U/audit"
sleep 1
echo
echo "Findings (pyo-waf's log lines):"
docker logs waf-rules-proc 2>/dev/null | grep '"waf"' | python3 -c '
import json, sys
for l in sys.stdin:
    h = json.loads(l)
    what = "blocked" if h["blocked"] else "logged "
    rules = list(zip(h["rules"], h["messages"]))[:3]
    print("  %s %15s  %-4s %-40s %s" % (what, h["client"], h["method"], h["uri"][:40], rules))
'
rm -rf "$DIR"
echo
[ "$fails" = 0 ] && echo "all passed" || { echo "$fails failed"; exit 1; }
