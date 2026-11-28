#!/usr/bin/env bash
# Fails when an image Pyo runs would come from Docker Hub, or from any
# registry not on the list below. A bare name (registry:3.1.2) or one
# without a registry (alpine/socat:1.8.1.3) is a Docker Hub image too.
# Docker Hub limits anonymous pulls, so a fresh install on a shared
# address can fail. Third-party images come from Pyo's mirror instead
# (deploy/mirrors.txt), and every mirror used must be listed there with
# the same digest. CI runs it (ci.yml).
#
# It reads what Pyo ships: the manifests (deploy/k8s, the system bundles),
# the install scripts, and the Rust code outside its tests.
set -euo pipefail
cd "$(dirname "$0")/.."

# Where Pyo's images may come from:
# - ghcr.io: Pyo's own and its mirrors, CloudNativePG's and MOCO's
# - nvcr.io: NVIDIA's device plugin, on GPU servers only
# - 127.0.0.1:5005: the local registry, in development (scripts/pyo-up.sh)
ALLOWED='^(ghcr\.io/|nvcr\.io/|127\.0\.0\.1:5005/)'
MIRROR=ghcr.io/upsetdev/pyo-mirror

# Rust files that handle the images of users' services (parsing a
# reference, logging in to its registry), so they name Docker Hub, and
# the API's examples of such a service. None picks an image Pyo runs.
USER_IMAGES='^crates/(pyo-api/src/registry\.rs|pyo-agent/src/rollout\.rs|pyo-sandbox/src/oci\.rs|pyo-api/src/openapi\.rs):'

# References Pyo writes but never pulls, kept as an older release had them
# so an update doesn't restart what runs. Each is matched by its exact
# line. CHART_SHUTDOWN_MANAGER_IMAGE: the gateway chart's shutdown
# manager, as 0.51.0's chart rendered it. The proxies run the image
# their EnvoyProxy names (ENVOY_GATEWAY_IMAGE, from the mirror).
NEVER_PULLED='^crates/pyo-proto/src/system\.rs:[0-9]+: pub const CHART_SHUTDOWN_MANAGER_IMAGE: &str = "docker\.io/envoyproxy/gateway:v1\.9\.3";$'

problems=()
problem() { problems+=("$1"); }

# An image reference: allowed, or a placeholder the release or the
# install fills in (__IMAGES__/…, $VAR, {image}).
check_ref() {
  local where=$1 ref=$2
  case "$ref" in __* | '$'* | '{'*) return ;; esac
  [[ "$ref" =~ $ALLOWED ]] || problem "$where: $ref doesn't come from Pyo's mirror or an allowed registry"
}

# The manifests and install scripts, without comment lines.
mapfile -t SHIPPED < <(git ls-files 'deploy/k8s/*.yaml' 'crates/pyo-proto/system/*.yaml' 'crates/pyo-proto/system/**/*.yaml' \
  'deploy/*.sh' 'crates/pyo-api/assets/*.sh')
[ "${#SHIPPED[@]}" -gt 0 ] || { echo "check-images: found nothing to check" >&2; exit 1; }
while IFS= read -r line; do
  where=${line%%: *}
  text=${line#*: }
  if [[ "$text" =~ (^|[[:space:]{-])image:[[:space:]]*[\"\']?([^\"\'[:space:]\#,}]+) ]]; then
    check_ref "$where" "${BASH_REMATCH[2]}"
  fi
  if [[ "$text" =~ _IMAGE[A-Z0-9_]*,[[:space:]]*value:[[:space:]]*[\"\']?([^\"\'[:space:]\#,}]+) ]]; then
    check_ref "$where" "${BASH_REMATCH[1]}"
  fi
  if [[ "$text" =~ docker\.io ]]; then problem "$where: names Docker Hub: $text"; fi
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
  rest=$text
  if [[ "${text,,}" == *image* ]]; then
    while [[ "$rest" =~ $REF ]]; do
      ref=${BASH_REMATCH[1]}
      rest=${rest#*"${BASH_REMATCH[0]}"}
      # A reference has a tag or a digest. Without, it's a name.
      [[ "$ref" == *:* || "$ref" == *@* ]] || continue
      [[ "$where" =~ $USER_IMAGES ]] || check_ref "$where" "$ref"
    done
  fi
  if [[ "$text" =~ \"[^\"]*docker\.io[^\"]*\" ]] && ! [[ "$where" =~ $USER_IMAGES ]]; then
    problem "$where: names Docker Hub: ${text#"${text%%[![:space:]]*}"}"
  fi
  if [[ "$text" =~ \"oci://([^\"]+)\" ]]; then check_ref "$where" "${BASH_REMATCH[1]}"; fi
done < <(awk '/^#\[cfg\(test\)\]/ { nextfile } !/^[ \t]*\/\// { print FILENAME ":" FNR ": " $0 }' "${RUST[@]}")

# Every mirror used anywhere in the repository is listed, with the same
# digest when it has one. A repository alone (a chart, a repository the
# version is added to) needs one of its tags listed. Every one listed is
# used.
mapfile -t LISTED < <(scripts/mirror-images.sh list | cut -d' ' -f2)
declare -A BY_TAG=() BY_REPO=() USED=()
for m in "${LISTED[@]}"; do
  BY_TAG[${m%@*}]=$m
  r=${m%@*}
  BY_REPO[${r%:*}]=1
done
while IFS= read -r line; do
  where=${line%%:*}
  ref=${line#*:}
  if [[ "$ref" == */ ]]; then
    continue # a prefix ("…/mirror/envoyproxy/"), not an image
  elif [[ "$ref" == *@* ]]; then
    [ "${BY_TAG[${ref%@*}]:-}" = "$ref" ] || problem "$where: $ref isn't in deploy/mirrors.txt with that digest"
    USED[${ref%@*}]=1
  elif [[ "${ref#"$MIRROR"/}" == *:* ]]; then
    [ -n "${BY_TAG[$ref]:-}" ] || problem "$where: $ref isn't in deploy/mirrors.txt"
    USED[$ref]=1
  else
    [ -n "${BY_REPO[$ref]:-}" ] || problem "$where: $ref isn't in deploy/mirrors.txt"
    USED[$ref]=1
  fi
done < <(git grep -oE "$MIRROR/[a-z0-9._/-]+(:[A-Za-z0-9._-]+)?(@sha256:[0-9a-f]{64})?" -- . ':!deploy/mirrors.txt' | sort -u)
for m in "${LISTED[@]}"; do
  tagged=${m%@*}
  [ -n "${USED[$tagged]:-}" ] || [ -n "${USED[${tagged%:*}]:-}" ] || problem "deploy/mirrors.txt: ${tagged#"$MIRROR"/} isn't used anywhere"
done

if [ "${#problems[@]}" -gt 0 ]; then
  printf '%s\n' "${problems[@]}" >&2
  echo "check-images: ${#problems[@]} image(s) would be pulled from outside Pyo's mirror or allowed registries. Mirror them in deploy/mirrors.txt and use the mirror." >&2
  exit 1
fi
echo "check-images: every image comes from Pyo's mirror or an allowed registry, and deploy/mirrors.txt matches what's used."
