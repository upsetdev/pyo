#!/usr/bin/env bash
# Fetches what the pyo-sandbox e2e tests boot: Firecracker and a guest
# kernel, both checked against pinned sha256 sums. Everything goes into
# .cache/sandbox-e2e at the repo root, which git ignores. Prints the env
# vars the tests read.
#
# The kernel is a Firecracker CI test artifact. Releases ship Pyo's own
# guest kernel (guest-kernel.sh) instead.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
cache=${PYO_SANDBOX_CACHE:-$root/.cache/sandbox-e2e}
# shellcheck source-path=SCRIPTDIR source=firecracker.env
. "$here/firecracker.env"
fc_version=$FC_VERSION
arch=x86_64
fc_tgz=firecracker-$fc_version-$arch.tgz
fc_sha=$FC_SHA256_X86_64
kernel=vmlinux-6.1.155
kernel_sha=e20e46d0c36c55c0d1014eb20576171b3f3d922260d9f792017aeff53af3d4f2

mkdir -p "$cache/dl" "$cache/bin"
fetch() { # url file sha
  if [ ! -f "$cache/dl/$2" ] || ! echo "$3  $cache/dl/$2" | sha256sum -c --quiet - 2>/dev/null; then
    curl -fsSL --retry 3 -o "$cache/dl/$2.part" "$1"
    mv "$cache/dl/$2.part" "$cache/dl/$2"
  fi
  echo "$3  $cache/dl/$2" | sha256sum -c --quiet -
}
fetch "https://github.com/firecracker-microvm/firecracker/releases/download/$fc_version/$fc_tgz" "$fc_tgz" "$fc_sha"
fetch "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/$arch/$kernel" "$kernel" "$kernel_sha"

if [ ! -x "$cache/bin/firecracker" ]; then
  x=$(mktemp -d "$cache/x.XXXXXX")
  tar -xzf "$cache/dl/$fc_tgz" -C "$x"
  rel="$x/release-$fc_version-$arch"
  (cd "$rel" && sha256sum -c --quiet SHA256SUMS)
  install -m 0755 "$rel/firecracker-$fc_version-$arch" "$cache/bin/firecracker"
  install -m 0755 "$rel/jailer-$fc_version-$arch" "$cache/bin/jailer"
  rm -rf "$x"
fi
cp -n "$cache/dl/$kernel" "$cache/vmlinux" 2>/dev/null || true

echo "PYO_SANDBOX_FIRECRACKER=$cache/bin/firecracker"
echo "PYO_SANDBOX_KERNEL=$cache/vmlinux"
