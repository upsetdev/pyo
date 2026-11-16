#!/usr/bin/env bash
# Fetches runsc for the pyo-sandbox gVisor e2e test: the release pinned in
# gvisor.env, checked against its pinned sha512. Everything goes into
# .cache/sandbox-gvisor at the repo root, which git ignores. Prints the env
# var the test reads.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
cache=${PYO_SANDBOX_GVISOR_CACHE:-$root/.cache/sandbox-gvisor}
# shellcheck source-path=SCRIPTDIR source=gvisor.env
. "$here/gvisor.env"
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 sum=$GVISOR_SHA512_X86_64 ;;
  aarch64 | arm64) arch=aarch64 sum=$GVISOR_SHA512_AARCH64 ;;
  *) echo "gVisor runs on x86-64 or arm64." >&2; exit 1 ;;
esac
tgz=$cache/dl/gvisor-$GVISOR_RELEASE-$arch.tar.zstd
mkdir -p "$cache/dl"
if [ ! -f "$tgz" ] || ! echo "$sum  $tgz" | sha512sum -c --quiet - 2>/dev/null; then
  curl -fsSL --proto '=https' --retry 3 -o "$tgz.part" \
    "https://storage.googleapis.com/gvisor/releases/release/$GVISOR_RELEASE/$arch/gvisor.tar.zstd"
  mv "$tgz.part" "$tgz"
fi
echo "$sum  $tgz" | sha512sum -c --quiet -
bin=$cache/$GVISOR_RELEASE-$arch
if [ ! -x "$bin/runsc" ]; then
  rm -rf "$bin.tmp"
  mkdir -p "$bin.tmp"
  tar --zstd -xf "$tgz" -C "$bin.tmp" runsc gvisor-bin
  rm -rf "$bin"
  mv "$bin.tmp" "$bin"
fi
echo "PYO_SANDBOX_RUNSC=$bin/runsc"
