#!/usr/bin/env bash
# Moves Pyo's pinned GeoLite2 databases to a newer one: GeoLite2 Country
# (deploy/images/geoip.Dockerfile, the proxies' image) and GeoLite2 City
# (deploy/images/pyo.Dockerfile, bundled with Pyo).
#
#   scripts/geoip-pin.sh             # the newest one published at least 14 days ago
#   scripts/geoip-pin.sh COMMIT      # the one at that upstream commit
#
# Upstream is github.com/GitSquared/node-geolite2-redist, which republishes
# MaxMind's databases about twice a week and force-pushes its master each
# time. A tarball is only in the commit that was master's head after that
# push, so the commits come from GitHub's record of the pushes, not from
# master's history. This downloads both tarballs at one commit, checks each
# against the .sha256 beside it and reads the database's date from its
# folder's name. Then it writes the date, the commit and the two sums on
# the ARG GEOIP_ lines of both Dockerfiles. It changes nothing else: see
# RELEASING.md, "Bump the GeoIP database", for the rest.
set -euo pipefail
cd "$(dirname "$0")/.."

REPO=GitSquared/node-geolite2-redist
MIN_AGE_DAYS=14
fail() { echo "geoip-pin: $*" >&2; exit 1; }
for tool in curl jq sha256sum tar; do
  command -v "$tool" >/dev/null || fail "$tool isn't installed"
done

commit=${1:-}
if [ -z "$commit" ]; then
  before=$(date -u -d "$MIN_AGE_DAYS days ago" +%Y-%m-%dT%H:%M:%SZ)
  commit=$(curl -fsSL --proto '=https' --max-time 60 "https://api.github.com/repos/$REPO/activity?ref=refs/heads/master&per_page=100" |
    jq -r --arg before "$before" '[.[] | select(.ref == "refs/heads/master" and .timestamp <= $before)][0].after // empty') ||
    fail "couldn't read upstream's pushes from GitHub"
  [ -n "$commit" ] || fail "GitHub lists no push to upstream's master from before $before"
fi
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || fail "$commit isn't a full commit id"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
base="https://raw.githubusercontent.com/$REPO/$commit/redist"
declare -A SUM=() DAY=()
for db in Country City; do
  echo "Downloading GeoLite2 $db…"
  curl -fsSL --proto '=https' --max-time 600 -o "$work/$db.tar.gz" "$base/GeoLite2-$db.tar.gz" ||
    fail "upstream has no GeoLite2-$db.tar.gz at $commit (a commit that was never master's head has none)"
  want=$(curl -fsSL --proto '=https' --max-time 60 "$base/GeoLite2-$db.tar.gz.sha256" | tr -d '[:space:]') ||
    fail "upstream has no GeoLite2-$db.tar.gz.sha256 at $commit"
  SUM[$db]=$(sha256sum "$work/$db.tar.gz" | cut -d' ' -f1)
  [[ "$want" =~ ^[0-9a-f]{64}$ ]] || fail "upstream's GeoLite2-$db.tar.gz.sha256 isn't a SHA-256: $want"
  [ "${SUM[$db]}" = "$want" ] || fail "GeoLite2-$db.tar.gz is ${SUM[$db]}, but upstream's .sha256 says $want"
  folder=$(tar tzf "$work/$db.tar.gz" | awk -F/ 'NR == 1 { print $1 }')
  DAY[$db]=${folder#"GeoLite2-${db}_"}
  [[ "${DAY[$db]}" =~ ^[0-9]{8}$ ]] || fail "GeoLite2-$db.tar.gz's folder has no date: $folder"
done
[ "${DAY[Country]}" = "${DAY[City]}" ] ||
  fail "at $commit, Country is from ${DAY[Country]} and City from ${DAY[City]}. Pick a commit where both are from the same day."
day=${DAY[Country]}
date="${day:0:4}-${day:4:2}-${day:6:2}"

pin() {
  local file=$1 name=$2 value=$3
  [ "$(grep -c "^ARG $name=" "$file")" = 1 ] || fail "$file needs exactly one ARG $name= line"
  sed -i "s|^ARG $name=.*|ARG $name=$value|" "$file"
}
for file in deploy/images/geoip.Dockerfile deploy/images/pyo.Dockerfile; do
  pin "$file" GEOIP_DATE "$date"
  pin "$file" GEOIP_COMMIT "$commit"
done
pin deploy/images/geoip.Dockerfile GEOIP_COUNTRY_SHA256 "${SUM[Country]}"
pin deploy/images/pyo.Dockerfile GEOIP_CITY_SHA256 "${SUM[City]}"

echo "Pinned the GeoLite2 databases of $date (upstream commit $commit)."
echo "  Country  ${SUM[Country]}"
echo "  City     ${SUM[City]}"
echo "The proxies' image is now geoip:$date. Record it, with why it changed:"
echo "  PYO_INBOUND_PODS_NOTE='GeoIP database of $date' cargo test -p pyo-proto --test inbound_pods"
