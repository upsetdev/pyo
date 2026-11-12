# The WAF's processor

Pyo's built-in web application firewall (set per service; it runs on a
cluster while a service there has it on): Coraza with
the OWASP Core Rule Set (v4, compiled in through `coraza-coreruleset`), as
an Envoy external processor (ext_proc, gRPC on :9002). It runs as `pyo-waf`
in the `pyo-waf` namespace. Pyo's Gateway sends it each request to a
service that has the WAF on; it runs that host's custom rules
(`cmd/pyo-waf/rules.go`), then its OWASP rules, and answers "go on" or a
403, and writes one JSON line per request where rules matched:

    {"waf":"pyo","time":"…","host":"shop.example.com","client":"203.0.113.9",
     "method":"GET","uri":"/?id=1' OR '1'='1","blocked":true,
     "rules":[942100,949110],"messages":["SQL Injection Attack Detected via libinjection",…],
     "severity":"critical","set":"s0"}

Pyo's Vector forwards those lines to the API (ClickHouse `waf_hits`).

## Settings

`/etc/pyo-waf/config.json` (a ConfigMap Pyo's agent writes). With
`-configmap pyo-waf` the processor follows that ConfigMap through the
Kubernetes API too (its role reads only it), so a change is in effect within
a second, not on the kubelet's next sync of the mounted file; the file,
re-read every 5 seconds when it changes, is the fallback while the API
can't be read:

    {"sets":  {"s0": {"engine": "On", "level": 1, "exceptions": [942100]},
               "s1": {"engine": "DetectionOnly", "level": 2}},
     "hosts": {"shop.example.com": "s0", "blog.example.com": "s1"}}

- `engine`: `On` blocks (Block mode), `DetectionOnly` only logs (Log only).
- `level`: the Core Rule Set's paranoia level, 1-4 (the dashboard's
  "Strictness").
- `exceptions`: rule ids removed for that set (`SecRuleRemoveById`).
- `skip`: paths the OWASP rules skip, per host.
- `lists` and `hostRules`: custom rules, and which host runs which list.
  Each rule is `{"id", "name", "match": "all"|"any", "conditions":
  [{"field", "name", "op", "values", "negate"}], "action":
  "block"|"log"|"allow"|"skipOwasp"}`. They're evaluated in Go, as data
  (plain comparisons and RE2 regexes), never turned into rule-language
  text. A rule that doesn't check out is left out, with an error logged.
  Matches are logged with the rule's id and its name as the message.
- `-geoip <file>`: a MaxMind country database, for `country` conditions.
- A host with no set and no rules isn't inspected.

Each distinct set is compiled once and shared by all requests (about 20-200
ms each at load), so memory grows with the number of distinct settings,
not with domains or traffic.

## When it decides

What doesn't need the body (custom rules on the request line, headers,
address and country; OWASP rules in Block mode on the request line and
headers) is decided on the headers message, so a request with a body is
refused before any of it is forwarded. The body then comes as Pyo's
route policy says (`waf_ext_proc` in pyo-proto's `system.rs`; the processor
reads the mode from the stream's `protocol_config`):

- Block mode: `BUFFERED_PARTIAL`. The proxy holds the request, headers
  included, and sends the body's first part (up to its 256 KiB buffer,
  Pyo's client policy) as one message, decided on even when the body
  goes on.
- Any other: `STREAMED`. Each piece is answered at once and passed on.
  Custom rules decide when the body ends; the OWASP rules, which only log
  there, run in the background after the answer (a few at once, findings
  dropped when too many wait). Envoy cancels a message's timeout when it
  gets the answer to an earlier piece, so a slow verdict on the last piece
  would hold the request however long it took, past the 500 ms timeout.

Either way, rules see a body's first 128 KiB (`inspectBody`): the OWASP
rules' time grows faster than a field's length (one field: about 1 s at
128 KiB, 6.5 s at 1 MB). A longer body is cut there, and Coraza treats it
as a body it cut itself (`partialMark`): a multipart body ending mid-part
isn't malformed, and one that doesn't parse for being cut isn't refused
for it. The rest passes uninspected.

## Why ext_proc and not Wasm

Measured with Envoy v1.39.5 (the one Envoy Gateway v1.9.3 runs), 2 CPUs,
2 worker threads, a `direct_response` route, keep-alive GETs
(`test/bench.py`):

| | Envoy memory | WAF memory | p50 | p99 |
|---|---|---|---|---|
| No WAF | 14.6 MiB | – | 0.25 ms | 0.75 ms |
| coraza-proxy-wasm 0.6.0, 1 rule set | 321 MiB | (inside Envoy) | 3.27 ms | 8.65 ms |
| coraza-proxy-wasm 0.6.0, 5 rule sets | 461 MiB | (inside Envoy) | 3.08 ms | 13.27 ms |
| pyo-waf (ext_proc), 4 rule sets | 16.1 MiB | 51-78 MiB | 2.27 ms | 4.40 ms |

The Wasm build keeps a VM per Envoy worker thread, each with its own copy
of the rules: too much for a 2 GB server, and it grows with CPUs.

## Build and test

    ./build.sh                       # go vet, go test, bin/pyo-waf (Go in docker)
    docker build -f deploy/images/waf.Dockerfile -t pyo-waf .   # from the repository root

`test/` runs it against a real Envoy in docker:

    python3 test/envoy.py <dir> /waf/coraza-proxy-wasm.wasm   # configs
    cp bin/pyo-waf <dir>/ ; test/run.sh <dir>                 # Envoys (Wasm comparison)
    test/probe.sh 19183                                       # status codes per mode
    python3 test/bench.py 19183 block.test 3000               # latency
    test/rules.sh <config.json> <GeoLite2-Country.mmdb>       # custom rules, end to end

`rules.sh` takes the settings pyo-api's `waf_harness_config` test writes
(`PYO_WAF_CONFIG=<file> cargo test -p pyo-api waf_harness_config -- --ignored`).
`go test -fuzz=FuzzValues ./cmd/pyo-waf` fuzzes rule values.

## Release

It ships with Pyo, at Pyo's version: the release workflow
(`.github/workflows/release.yml`, see `RELEASING.md`) runs these tests,
builds and signs `ghcr.io/upsetdev/pyo/waf:X.Y.Z` with Pyo's other images,
and pins its digest in `release.json`; installs set `PYO_WAF_IMAGE` from it.
Locally, `scripts/pyo-up.sh` builds it with the rest.
