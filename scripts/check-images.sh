#!/usr/bin/env bash
# Checks where the images Pyo runs come from.
#
#   scripts/check-images.sh            # the repository (CI's Image sources job)
#   scripts/check-images.sh list       # each third-party image, one per line
#   scripts/check-images.sh upstream   # each one's digest is still at Docker Hub (read only, needs crane)
#
# With no argument it fails when an image Pyo ships comes from a registry
# not on the list below, or from Docker Hub without being listed in
# deploy/mirrors.txt with the same digest. A bare name (registry:3.1.2) or
# one without a registry (alpine/socat:1.8.1.3) fails too: Pyo writes
# docker.io/ in full. Every image listed must be used.
#
# It reads what Pyo ships: the manifests (deploy/k8s, the system bundles),
# the install scripts, and the Rust code outside its tests.
set -euo pipefail
cd "$(dirname "$0")/.."
LIST=deploy/mirrors.txt

# Where Pyo's images may come from:
# - ghcr.io: Pyo's own, CloudNativePG's and MOCO's
# - docker.io: the third-party images in deploy/mirrors.txt
# - nvcr.io: NVIDIA's device plugin, on GPU servers only
# - 127.0.0.1:5005: the local registry, in development (scripts/pyo-up.sh)
ALLOWED='^(ghcr\.io/|docker\.io/|nvcr\.io/|127\.0\.0\.1:5005/)'
HUB=docker.io

die() { echo "check-images: $*" >&2; exit 1; }

# Each line of the list: docker.io/<path>:<tag>@sha256:<digest>.
LISTED=()
while read -r src rest; do
  case "$src" in "" | "#"*) continue ;; esac
  [ -z "$rest" ] || die "$LIST: one image per line: $src $rest"
  [[ "$src" =~ ^docker\.io/[a-z0-9._/-]+:[A-Za-z0-9._-]+@sha256:[0-9a-f]{64}$ ]] \
    || die "$LIST: not docker.io/<path>:<tag>@sha256:<digest>: $src"
  LISTED+=("$src")
done <"$LIST"
[ "${#LISTED[@]}" -gt 0 ] || die "$LIST lists no image"
dupes=$(printf '%s\n' "${LISTED[@]%@*}" | sort | uniq -d)
[ -z "$dupes" ] || die "$LIST lists these twice: $dupes"

# crane, retried: Docker Hub answers 429 now and then.
retry() {
  local n
  for n in 1 2 3 4 5; do
    "$@" && return 0
    [ "$n" = 5 ] || { echo "  failed, trying again in $((n * 15)) s" >&2; sleep $((n * 15)); }
  done
  return 1
}

case "${1:-}" in
  list)
    printf '%s\n' "${LISTED[@]}"
    exit 0
    ;;
  upstream)
    for src in "${LISTED[@]}"; do
      digest=${src#*@}
      repo=${src%@*}
      repo=${repo%:*}
      got=$(retry crane digest "$repo@$digest") || die "${src%@*}: its digest $digest can't be found on Docker Hub"
      [ "$got" = "$digest" ] || die "${src%@*}: Docker Hub answers $got for $digest"
      # A tag that moved upstream is fine: the list pins the digest.
      now=$(retry crane digest "${src%@*}" 2>/dev/null) || now=""
      if [ -n "$now" ] && [ "$now" != "$digest" ]; then
        echo "${src%@*}  ok (the tag points at $now now)"
      else
        echo "${src%@*}  ok"
      fi
    done
    exit 0
    ;;
  "") ;;
  *) die "usage: $0 [list | upstream]" ;;
esac

# Rust files that handle the images of users' services (parsing a
# reference, logging in to its registry), so they name Docker Hub, and
# the API's examples of such a service. None picks an image Pyo runs.
USER_IMAGES='^crates/(pyo-api/src/registry\.rs|pyo-agent/src/rollout\.rs|pyo-sandbox/src/oci\.rs|pyo-api/src/openapi\.rs):'

# References Pyo writes but never pulls, kept as an older release had them
# so an update doesn't restart what runs. Each is matched by its exact
# line. CHART_SHUTDOWN_MANAGER_IMAGE: the gateway chart's shutdown
# manager, as 0.51.0's chart rendered it. The proxies run the image
# their EnvoyProxy names (ENVOY_GATEWAY_IMAGE, pinned).
NEVER_PULLED='^crates/pyo-proto/src/system\.rs:[0-9]+: pub const CHART_SHUTDOWN_MANAGER_IMAGE: &str = "docker\.io/envoyproxy/gateway:v1\.9\.3";$'

problems=()
problem() { problems+=("$1"); }

declare -A BY_TAG=() BY_REPO=() USED=()
for m in "${LISTED[@]}"; do
  BY_TAG[${m%@*}]=$m
  r=${m%@*}
  BY_REPO[${r%:*}]=1
done

# An image reference: allowed, or a placeholder the release or the
# install fills in (__IMAGES__/…, $VAR, {image}). One from Docker Hub is
# listed: with its digest when it has one, by its tag, or (a repository
# the version is added to, a chart) by one of its tags.
check_ref() {
  local where=$1 ref=$2
  case "$ref" in __* | '$'* | '{'*) return ;; esac
  if ! [[ "$ref" =~ $ALLOWED ]]; then
    problem "$where: $ref doesn't come from an allowed registry"
    return
  fi
  [[ "$ref" == "$HUB/"* ]] || return 0
  if [[ "$ref" == *@* ]]; then
    [ "${BY_TAG[${ref%@*}]:-}" = "$ref" ] || problem "$where: $ref isn't in $LIST with that digest"
    USED[${ref%@*}]=1
  elif [[ "${ref#"$HUB"/}" == *:* ]]; then
    [ -n "${BY_TAG[$ref]:-}" ] || problem "$where: $ref isn't in $LIST"
    USED[$ref]=1
  else
    [ -n "${BY_REPO[$ref]:-}" ] || problem "$where: $ref isn't in $LIST"
    USED[$ref]=1
  fi
}

# The manifests and install scripts, without comment lines.
mapfile -t SHIPPED < <(git ls-files 'deploy/k8s/*.yaml' 'crates/pyo-proto/system/*.yaml' 'crates/pyo-proto/system/**/*.yaml' \
  'deploy/*.sh' 'crates/pyo-api/assets/*.sh')
[ "${#SHIPPED[@]}" -gt 0 ] || die "found nothing to check"
while IFS= read -r line; do
  where=${line%%: *}
  text=${line#*: }
  if [[ "$text" =~ (^|[[:space:]{-])image:[[:space:]]*[\"\']?([^\"\'[:space:]\#,}]+) ]]; then
    check_ref "$where" "${BASH_REMATCH[2]}"
  fi
  if [[ "$text" =~ _IMAGE[A-Z0-9_]*,[[:space:]]*value:[[:space:]]*[\"\']?([^\"\'[:space:]\#,}]+) ]]; then
    check_ref "$where" "${BASH_REMATCH[1]}"
  fi
  if [[ "$text" =~ oci://([^\"\'[:space:]]+) ]]; then check_ref "$where" "${BASH_REMATCH[1]}"; fi
done < <(awk '!/^[ \t]*#/ { print FILENAME ":" FNR ": " $0 }' "${SHIPPED[@]}")

# The Rust code, without its tests (a crate's tests/, and each file from
# its #[cfg(test)] module on) and comments: image references on lines
# about images, Docker Hub anywhere, charts.
mapfile -t RUST < <(git ls-files 'crates/*/src/*.rs' 'crates/*/src/**/*.rs' \
  | grep -vE '/tests?/|/tests?\.rs$|_tests?\.rs$|/e2e\.rs$|/fake[a-z]*\.rs$')
REF='"(([a-z0-9.-]+(:[0-9]+)?/)?[a-z0-9._-]+(/[a-z0-9._-]+)*(:[A-Za-z0-9._-]+)?(@sha256:[0-9a-f]{64})?)"'
while IFS= read -r line; do
  [[ "$line" =~ $NEVER_PULLED ]] && continue
  where=${line%%: *}
  text=${line#*: }
  [[ "$where" =~ $USER_IMAGES ]] && continue
  rest=$text
  if [[ "${text,,}" == *image* ]]; then
    while [[ "$rest" =~ $REF ]]; do
      ref=${BASH_REMATCH[1]}
      rest=${rest#*"${BASH_REMATCH[0]}"}
      # A reference has a tag or a digest. Without, it's a name.
      [[ "$ref" == *:* || "$ref" == *@* ]] || continue
      check_ref "$where" "$ref"
    done
  fi
  # Docker Hub on a line that isn't about images (an engine's tools, a
  # repository the version is added to): listed all the same.
  if [[ "${text,,}" != *image* ]]; then
    hub="\"($HUB/[a-z0-9._/-]+(:[A-Za-z0-9._-]+)?(@sha256:[0-9a-f]{64})?)\""
    while [[ "$rest" =~ $hub ]]; do
      check_ref "$where" "${BASH_REMATCH[1]}"
      rest=${rest#*"${BASH_REMATCH[0]}"}
    done
  fi
  if [[ "$text" =~ \"oci://([^\"]+)\" ]]; then check_ref "$where" "${BASH_REMATCH[1]}"; fi
done < <(awk '/^#\[cfg\(test\)\]/ { nextfile } !/^[ \t]*\/\// { print FILENAME ":" FNR ": " $0 }' "${RUST[@]}")

# Every image listed is used.
for m in "${LISTED[@]}"; do
  tagged=${m%@*}
  [ -n "${USED[$tagged]:-}" ] || [ -n "${USED[${tagged%:*}]:-}" ] || problem "$LIST: $tagged isn't used anywhere"
done

if [ "${#problems[@]}" -gt 0 ]; then
  printf '%s\n' "${problems[@]}" >&2
  echo "check-images: ${#problems[@]} problem(s). Third-party images come from Docker Hub, written in full and listed in $LIST with their digest. Pyo's own come from ghcr.io." >&2
  exit 1
fi
echo "check-images: every image comes from an allowed registry, and $LIST matches what's used."
