#!/usr/bin/env bash
# Packs a sandbox server's files into the release's
# pyo-sandbox-linux-<arch>.tar.gz, with its .sha256 beside it: what
# deploy/sandbox.sh downloads (or takes with --bundle) and installs.
#
#   crates/pyo-sandbox/scripts/bundle.sh --arch x86_64 \
#     --sandbox target/x86_64-unknown-linux-musl/release/pyo-sandbox \
#     --envd target/x86_64-unknown-linux-musl/release/pyo-envd \
#     --kernel out/vmlinux --out dist
#
# The daemon and pyo-envd come built (static, musl), the kernel from
# guest-kernel.sh. Firecracker and its jailer are downloaded here, the
# release pinned in firecracker.env, and checked against its pinned sha256
# and the release's own SHA256SUMS. The tarball is flat: pyo-sandbox,
# pyo-envd, firecracker, jailer and vmlinux.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source-path=SCRIPTDIR source=firecracker.env
. "$here/firecracker.env"

arch="" sandbox="" envd="" kernel="" out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) arch=${2:-}; shift 2 ;;
    --sandbox) sandbox=${2:-}; shift 2 ;;
    --envd) envd=${2:-}; shift 2 ;;
    --kernel) kernel=${2:-}; shift 2 ;;
    --out) out=${2:-}; shift 2 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done
case "$arch" in
  x86_64) fc_sha=$FC_SHA256_X86_64 ;;
  aarch64) fc_sha=$FC_SHA256_AARCH64 ;;
  *) echo "--arch is x86_64 or aarch64." >&2; exit 2 ;;
esac
for f in "$sandbox" "$envd" "$kernel"; do
  [ -n "$f" ] && [ -s "$f" ] || { echo "Give --sandbox, --envd and --kernel as files (\"$f\" isn't one)." >&2; exit 2; }
done
[ -n "$out" ] || { echo "Give --out, the directory for the tarball." >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fc_tgz=firecracker-$FC_VERSION-$arch.tgz
curl -fsSL --proto '=https' --retry 3 -o "$work/$fc_tgz" \
  "https://github.com/firecracker-microvm/firecracker/releases/download/$FC_VERSION/$fc_tgz"
echo "$fc_sha  $work/$fc_tgz" | sha256sum -c --quiet - || { echo "$fc_tgz doesn't match its pinned sha256." >&2; exit 1; }
mkdir "$work/fc"
tar -xzf "$work/$fc_tgz" -C "$work/fc"
rel=$work/fc/release-$FC_VERSION-$arch
(cd "$rel" && sha256sum -c --quiet SHA256SUMS) || { echo "Firecracker's files don't match its SHA256SUMS." >&2; exit 1; }

b=$work/bundle
mkdir "$b"
install -m 755 "$sandbox" "$b/pyo-sandbox"
install -m 755 "$envd" "$b/pyo-envd"
install -m 755 "$rel/firecracker-$FC_VERSION-$arch" "$b/firecracker"
install -m 755 "$rel/jailer-$FC_VERSION-$arch" "$b/jailer"
install -m 644 "$kernel" "$b/vmlinux"

name=pyo-sandbox-linux-$arch.tar.gz
mkdir -p "$out"
# The same files give the same tarball: fixed order, owner and times.
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 -C "$b" -cf - \
  pyo-sandbox pyo-envd firecracker jailer vmlinux | gzip -n -9 > "$out/$name"
(cd "$out" && sha256sum "$name" > "$name.sha256")
(cd "$b" && ls -l --block-size=K pyo-sandbox pyo-envd firecracker jailer vmlinux)
echo "$out/$name: $(du -h "$out/$name" | cut -f1)"
cat "$out/$name.sha256"
