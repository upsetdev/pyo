#!/usr/bin/env bash
# Checks that firewall changes change only a website's routes, never a
# listener's HTTP filters. Envoy drops a listener's open connections
# (after its drain timeout) when its filters change.
#
# Writes the gateway objects Pyo's agent makes before and after each kind
# of change (pyo-agent's `drain_scenarios` test), translates each with
# Envoy Gateway's egctl (the version Pyo installs, checksum checked), and
# compares the listeners. Also shows the same changes as Pyo wrote them
# before 0.51.1, and what the updates themselves change once: from before
# 0.51.1, and from the release before the Gateway section (its anchor
# routes and the policy on Pyo's own tailnet routes). Firewall changes,
# traffic paths and the Gateway section's settings (compression, the
# timeout defaults, HTTP/3, the TLS minimum) are all in it. Needs cargo,
# curl, openssl and python3. Runs offline after the download, no cluster.
#
#   ./scripts/gateway-drain-check.sh
#
# SCENARIOS=<dir>: scenarios already written there (PYO_DRAIN_SCENARIOS=<dir>
# cargo test -p pyo-agent drain_scenarios -- --ignored), without cargo.
#
# Exits 1 when a change that should change only routes changes a listener.
set -euo pipefail

EG_VERSION=v1.9.3
case "$(uname -m)" in
  x86_64) ARCH=amd64 SHA=82b6164a6cd966fcb6e244a5931c6c3e46193816714a0b69043db1606098150f ;;
  aarch64 | arm64) ARCH=arm64 SHA=6c8f6e880fe27db3adab5778098bc668f71576cd6c1692e1310c07d7d60e9291 ;;
  *) echo "No egctl for $(uname -m)." >&2; exit 2 ;;
esac
HERE="$(cd "$(dirname "$0")" && pwd)"
PY="python3 -I $HERE/gateway-drain-check.py"
DIR="$(mktemp -d)"
trap 'rm -rf "$DIR"' EXIT

echo "Downloading egctl $EG_VERSION"
TAR="egctl_${EG_VERSION}_linux_${ARCH}.tar.gz"
mkdir -p "$DIR/dl" "$DIR/egctl"
curl -fsSL -o "$DIR/dl/$TAR" "https://github.com/envoyproxy/gateway/releases/download/$EG_VERSION/$TAR"
echo "$SHA  $DIR/dl/$TAR" | sha256sum -c --quiet
tar -xzf "$DIR/dl/$TAR" -C "$DIR/egctl"
EGCTL="$DIR/egctl/bin/linux/$ARCH/egctl"

if [ -n "${SCENARIOS:-}" ]; then
  cp -r "$SCENARIOS" "$DIR/in"
else
  echo "Writing the scenarios"
  mkdir -p "$DIR/in"
  (cd "$HERE/.." && PYO_DRAIN_SCENARIOS="$DIR/in" cargo test -q -p pyo-agent drain_scenarios -- --ignored >/dev/null)
fi
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$DIR/key.pem" -out "$DIR/cert.pem" \
  -days 2 -subj "/CN=example.test" 2>/dev/null

n=$(ls "$DIR/in" | wc -l)
for when in before previous after; do
  mkdir -p "$DIR/$when"
  i=0
  for f in "$DIR"/in/*.json; do
    i=$((i + 1))
    name=$(basename "$f" .json)
    printf '\rTranslating (%s) %d/%d' "$when" "$i" "$n"
    flag=""
    [ "$when" = before ] && flag=--before
    [ "$when" = previous ] && flag=--previous
    $PY yaml "$f" "$DIR/cert.pem" "$DIR/key.pem" "$DIR/$when/$name.yaml" $flag
    "$EGCTL" x translate --from gateway-api --to xds -t all -o json \
      -f "$DIR/$when/$name.yaml" >"$DIR/$when/$name.json" 2>"$DIR/$when/$name.err" ||
      { echo; echo "egctl failed on $name ($when):"; cat "$DIR/$when/$name.err"; exit 2; }
  done
  echo
done

echo
echo "== Before (each route's policies only while its firewall has rules)"
$PY compare "$DIR/before" || true
echo
echo "== The update itself (once, when a service's routes are next applied)"
$PY rollout "$DIR/before" "$DIR/after"
echo
echo "== This release's update (once): the release before the Gateway section, then now"
$PY rollout "$DIR/previous" "$DIR/after"
echo
echo "== Now"
$PY compare "$DIR/after"
