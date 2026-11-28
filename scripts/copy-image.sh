#!/usr/bin/env bash
# Copies one of Pyo's signed images to another name, with its signature,
# and checks the copy:
#
#   scripts/copy-image.sh <repo>@sha256:<digest> <repo>:<tag>
#
#   scripts/copy-image.sh ghcr.io/upsetdev/pyo-agent@sha256:… docker.io/upsetdev/pyo-agent:0.52.1
#
# The release workflow runs it for each image: to Docker Hub, and to the
# image's old nested name on ghcr.io (RELEASING.md, Image names).
#
# - The image goes as it is, every platform and its attestations (crane
#   copy: nothing rebuilt). One that's there already with that digest
#   isn't copied again.
# - Its signature goes too. cosign keeps it as a Sigstore bundle that
#   refers to the image, found under the tag sha256-<digest> (the OCI
#   referrers tag), so that tag is copied.
# - The source must carry the release workflow's signature. Then the
#   copy's digest must be the source's, and cosign must find the same
#   signature on the copy.
#
# It needs crane and cosign, logged in to both registries.
set -euo pipefail

SIGNER='^https://github\.com/upsetdev/pyo/\.github/workflows/release\.yml@refs/tags/v'
ISSUER=https://token.actions.githubusercontent.com

die() { echo "copy-image: $*" >&2; exit 1; }

[ $# = 2 ] || die "usage: $0 <repo>@sha256:<digest> <repo>:<tag>"
src=$1 dst=$2
[[ "$src" =~ ^([a-z0-9.:-]+/[a-z0-9._/-]+)@(sha256:[0-9a-f]{64})$ ]] || die "not <repo>@sha256:<digest>: $src"
src_repo=${BASH_REMATCH[1]} digest=${BASH_REMATCH[2]}
[[ "$dst" =~ ^([a-z0-9.:-]+/[a-z0-9._/-]+):([A-Za-z0-9._-]+)$ ]] || die "not <repo>:<tag>: $dst"
dst_repo=${BASH_REMATCH[1]}
sig_tag=sha256-${digest#sha256:}

# crane, retried: Docker Hub answers 429 now and then.
retry() {
  local n
  for n in 1 2 3 4 5; do
    "$@" && return 0
    [ "$n" = 5 ] || { echo "  failed, trying again in $((n * 15)) s" >&2; sleep $((n * 15)); }
  done
  return 1
}

# Only an image the release workflow signed is copied.
retry cosign verify "$src" --certificate-identity-regexp "$SIGNER" --certificate-oidc-issuer "$ISSUER" >/dev/null \
  || die "$src isn't signed by the release workflow, so it isn't copied"

if [ "$(crane digest "$dst" 2>/dev/null || true)" = "$digest" ]; then
  echo "$dst  there already"
else
  echo "$dst  copying from $src_repo"
  retry crane copy "$src" "$dst" || die "couldn't copy $src to $dst"
fi
got=$(retry crane digest "$dst") || die "$dst can't be read back"
[ "$got" = "$digest" ] || die "$dst has digest $got, not the source's $digest"

crane digest "$src_repo:$sig_tag" >/dev/null 2>&1 || die "$src has no signature under $src_repo:$sig_tag"
retry crane copy "$src_repo:$sig_tag" "$dst_repo:$sig_tag" || die "couldn't copy the signature of $src to $dst_repo"
retry cosign verify "$dst_repo@$digest" --certificate-identity-regexp "$SIGNER" --certificate-oidc-issuer "$ISSUER" >/dev/null \
  || die "$dst_repo@$digest has no signature from the release workflow after the copy"
echo "$dst  same digest, signed"
