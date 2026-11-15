#!/usr/bin/env bash
# Builds the guest kernel every sandbox VM boots: a pinned Linux 6.1 LTS
# from kernel.org with Firecracker's microVM config (and its CI additions)
# from the Firecracker release sandbox servers run. Everything it downloads
# is checked against the sha256 pinned here. It builds for the machine it
# runs on, and writes <out>/vmlinux (the image Firecracker boots: an ELF on
# x86-64, the Image on arm64) and <out>/vmlinux.config.
#
#   crates/pyo-sandbox/scripts/guest-kernel.sh <out>
#
# Needs gcc, make, flex, bison, bc, perl, libelf and OpenSSL headers
# (on Ubuntu: build-essential flex bison bc libelf-dev libssl-dev). The
# release workflow caches the result by this file and firecracker.env, so
# a change to either builds it again. To update the kernel: the new 6.1
# release and its sha256 from https://cdn.kernel.org/pub/linux/kernel/v6.x/sha256sums.asc.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source-path=SCRIPTDIR source=firecracker.env
. "$here/firecracker.env"

KERNEL_VERSION=6.1.192
KERNEL_SHA256=1e88e317e20dbc79a2cb9e3d9667e908b262d12e76eb66eed894c5460eb87fca
# Firecracker's guest configs at $FC_VERSION (resources/guest_configs).
CONFIG_X86_64_SHA256=153ca1b40f3312bfb40b7587b471ea548a915f98f480f0d0892a1537957a2147
CONFIG_AARCH64_SHA256=5c73202d00b5591ad987d9f808208a318e317fd32dbc17de613c5a93fabe2939
CI_CONFIG_SHA256=6ce47107d9fb695a873ffc4733a1d8e1b4941c4f272e20dac4bf163dfb77f47e
# What sandboxes rely on, built in (there are no modules): vsock for
# pyo-envd, virtio disks and network, ext4 and overlay for the root, and
# what Docker inside a sandbox needs.
REQUIRED="VIRTIO_VSOCKETS VIRTIO_BLK VIRTIO_NET EXT4_FS OVERLAY_FS DEVTMPFS
  CGROUPS MEMCG NAMESPACES USER_NS VETH BRIDGE NF_TABLES NF_NAT TUN SECCOMP
  VIRTIO_BALLOON PAGE_REPORTING"

out=${1:?usage: guest-kernel.sh <out dir>}
case "$(uname -m)" in
  x86_64) arch=x86_64 config_sha=$CONFIG_X86_64_SHA256 target=vmlinux image=vmlinux ;;
  aarch64 | arm64) arch=aarch64 config_sha=$CONFIG_AARCH64_SHA256 target=Image image=arch/arm64/boot/Image ;;
  *) echo "guest-kernel.sh builds on x86_64 or aarch64, not $(uname -m)." >&2; exit 1 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fetch() { # url file sha256
  curl -fsSL --proto '=https' --retry 3 -o "$work/$2" "$1"
  echo "$3  $work/$2" | sha256sum -c --quiet - || { echo "$1 doesn't match its pinned sha256." >&2; exit 1; }
}
configs=https://raw.githubusercontent.com/firecracker-microvm/firecracker/$FC_VERSION/resources/guest_configs
fetch "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KERNEL_VERSION.tar.xz" linux.tar.xz "$KERNEL_SHA256"
fetch "$configs/microvm-kernel-ci-$arch-6.1.config" microvm.config "$config_sha"
fetch "$configs/ci.config" ci.config "$CI_CONFIG_SHA256"

tar -xJf "$work/linux.tar.xz" -C "$work"
src=$work/linux-$KERNEL_VERSION
# Firecracker's own way: the configs one after the other, then olddefconfig
# settles what the newer 6.1 release added.
awk 1 "$work/microvm.config" "$work/ci.config" > "$src/.config"
export KBUILD_BUILD_USER=pyo KBUILD_BUILD_HOST=pyo KBUILD_BUILD_TIMESTAMP="Thu Jan  1 00:00:00 UTC 1970"
make -C "$src" -s olddefconfig
missing=""
for opt in $REQUIRED; do
  grep -qx "CONFIG_$opt=y" "$src/.config" || missing="$missing $opt"
done
[ -z "$missing" ] || { echo "The guest kernel's config lacks:$missing" >&2; exit 1; }
make -C "$src" -s -j"$(nproc)" "$target"

mkdir -p "$out"
install -m 644 "$src/$image" "$out/vmlinux"
install -m 644 "$src/.config" "$out/vmlinux.config"
echo "Linux $KERNEL_VERSION ($arch): $(du -h "$out/vmlinux" | cut -f1)"
