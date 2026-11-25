#!/usr/bin/env bash
# The tags of Pyo's images that a release doesn't tag with its version:
#
#   scripts/image-tags.sh           # "<image> <tag>", one per line
#   scripts/image-tags.sh waf       # that image's tag
#
#   geoip  the date of its database (GEOIP_DATE in
#          deploy/images/geoip.Dockerfile): 2026-11-06
#   waf    a hash of what it's built from: its Dockerfile, waf/go.mod,
#          waf/go.sum and waf/cmd without the tests
#
# The pods that run them (the proxies, the WAF, the traffic store) restart
# when their image's reference changes. With these tags it changes only
# when the image does. The release workflow publishes an image under its
# tag once and uses that one again while the tag stays the same, and
# scripts/release-manifest.ts pins it in release.json. pyo-proto's
# inbound_pods test holds both tags, so a change is made on purpose.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGES=(geoip waf)

tag_of() {
  local tag
  case "$1" in
    geoip)
      tag=$(sed -n 's/^ARG GEOIP_DATE=\(.*\)$/\1/p' deploy/images/geoip.Dockerfile)
      [[ "$tag" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] ||
        { echo "image-tags: deploy/images/geoip.Dockerfile needs one ARG GEOIP_DATE=YYYY-MM-DD line" >&2; return 1; }
      ;;
    waf)
      tag=$({
        printf '%s\n' deploy/images/waf.Dockerfile waf/go.mod waf/go.sum
        find waf/cmd -type f ! -name '*_test.go' ! -path '*/testdata/*'
      } | LC_ALL=C sort | xargs sha256sum | sha256sum | cut -c1-16)
      [[ "$tag" =~ ^[0-9a-f]{16}$ ]] || { echo "image-tags: couldn't hash the WAF's sources" >&2; return 1; }
      ;;
    *)
      echo "image-tags: $1 is tagged with Pyo's version (images with their own tag: ${IMAGES[*]})" >&2
      return 2
      ;;
  esac
  echo "$tag"
}

if [ $# -eq 0 ]; then
  for image in "${IMAGES[@]}"; do
    tag=$(tag_of "$image")
    echo "$image $tag"
  done
else
  tag_of "$1"
fi
