#!/usr/bin/env bash
# Copies the third-party images Pyo runs (deploy/mirrors.txt) to
# ghcr.io/upsetdev/pyo-mirror, so installs never pull from Docker Hub.
#
#   scripts/mirror-images.sh list           # each source and its mirror
#   scripts/mirror-images.sh check          # each source's digest is still there (read only)
#   scripts/mirror-images.sh copy [SUFFIX]  # copy each one, check it, check anyone can pull it
#
# copy takes every platform of the source's index as it is (crane copy:
# nothing rebuilt), then checks the mirror's digest is the source's.
# One that's there already with that digest isn't copied again. SUFFIX
# goes after each tag (copy -test: registry:3.1.2-test), for a trial run.
# It needs crane, logged in to ghcr.io with packages: write.
set -euo pipefail

cd "$(dirname "$0")/.."
LIST=deploy/mirrors.txt
MIRROR=ghcr.io/upsetdev/pyo-mirror

die() { echo "mirror-images: $*" >&2; exit 1; }

# Each source line: <registry>/<path>:<tag>@sha256:<digest>.
SOURCES=()
while read -r src rest; do
  case "$src" in "" | "#"*) continue ;; esac
  [ -z "$rest" ] || die "$LIST: one image per line: $src $rest"
  [[ "$src" =~ ^[a-z0-9.-]+\.[a-z]+/[a-z0-9._/-]+:[A-Za-z0-9._-]+@sha256:[0-9a-f]{64}$ ]] \
    || die "$LIST: not <registry>/<path>:<tag>@sha256:<digest>: $src"
  SOURCES+=("$src")
done <"$LIST"

# A source's repository, without its tag and digest.
repo_of() { local r=${1%@*}; echo "${r%:*}"; }

# The mirror of a source, by tag: its path without the registry and "library/".
mirror_of() {
  local path=${1%@*}
  path=${path#*/}
  path=${path#library/}
  echo "$MIRROR/$path"
}

# Anyone can pull it: an anonymous token, then the manifest by digest.
public() {
  local repo=${1#ghcr.io/} digest=$2 token code
  repo=${repo%:*}
  token=$(curl -fsS "https://ghcr.io/token?scope=repository:$repo:pull" | jq -r .token)
  code=$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" \
    -H 'Accept: application/vnd.oci.image.index.v1+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.docker.distribution.manifest.v2+json' \
    "https://ghcr.io/v2/$repo/manifests/$digest")
  [ "$code" = 200 ]
}

# crane, retried: Docker Hub answers 429 now and then.
retry() {
  local n
  for n in 1 2 3 4 5; do
    "$@" && return 0
    [ "$n" = 5 ] || { echo "  failed, trying again in $((n * 15)) s" >&2; sleep $((n * 15)); }
  done
  return 1
}

[ "${#SOURCES[@]}" -gt 0 ] || die "$LIST lists no image"
dupes=$(printf '%s\n' "${SOURCES[@]}" | while read -r s; do mirror_of "$s"; done | sort | uniq -d)
[ -z "$dupes" ] || die "$LIST: two sources have the same mirror: $dupes"

case "${1:-}" in
  list)
    for src in "${SOURCES[@]}"; do
      echo "$src $(mirror_of "$src")@${src#*@}"
    done
    ;;
  check)
    for src in "${SOURCES[@]}"; do
      digest=${src#*@}
      got=$(retry crane digest "$(repo_of "$src")@$digest") || die "${src%@*}: its digest $digest can't be found at the source"
      [ "$got" = "$digest" ] || die "${src%@*}: the source answers $got for $digest"
      # A tag that moved upstream is fine: the list pins the digest.
      now=$(retry crane digest "${src%@*}" 2>/dev/null) || now=""
      if [ -n "$now" ] && [ "$now" != "$digest" ]; then
        echo "${src%@*}  ok (the tag points at $now upstream now)"
      else
        echo "${src%@*}  ok"
      fi
    done
    ;;
  copy)
    suffix=${2:-}
    [[ "$suffix" =~ ^(-[a-z0-9.-]+)?$ ]] || die "a suffix starts with - (copy -test), not $suffix"
    private=()
    for src in "${SOURCES[@]}"; do
      digest=${src#*@}
      dst=$(mirror_of "$src")$suffix
      if [ "$(crane digest "$dst" 2>/dev/null || true)" = "$digest" ]; then
        echo "$dst  there already"
      else
        echo "$dst  copying from ${src%@*}"
        retry crane copy "$(repo_of "$src")@$digest" "$dst" || die "couldn't copy ${src%@*} to $dst"
      fi
      got=$(retry crane digest "$dst") || die "$dst can't be read back"
      [ "$got" = "$digest" ] || die "$dst has digest $got, not the source's $digest"
      public "$dst" "$digest" || private+=("$dst")
    done
    if [ "${#private[@]}" -gt 0 ]; then
      printf '::error::%s can'"'"'t be pulled without logging in. Make the package public in its settings on GitHub, then run this again.\n' "${private[@]}"
      exit 1
    fi
    ;;
  *)
    die "usage: $0 list | check | copy [SUFFIX]"
    ;;
esac
