#!/usr/bin/env bash
# run.sh DIR: starts three Envoys (no WAF :19180, WAF with five rule sets
# :19181, WAF with one :19182), 2 CPUs and 2 worker threads each, like a
# small server. Stop with: docker rm -f waf-base waf-coraza waf-one
set -euo pipefail
DIR=$1; IMG=envoyproxy/envoy:v1.39.5
docker rm -f waf-base waf-coraza waf-one >/dev/null 2>&1 || true
run(){ docker run -d --name "$1" --cpus 2 --memory 2g -p "127.0.0.1:$2:10000" -v "$DIR:/waf:ro" $IMG -c "/waf/$3" --concurrency 2 >/dev/null; }
run waf-base 19180 base.json; run waf-coraza 19181 waf.json; run waf-one 19182 waf1.json
for p in 19180 19181 19182; do timeout 120 bash -c "until curl -s -o /dev/null http://127.0.0.1:$p/; do sleep 1; done"; done
