#!/usr/bin/env bash
# Installs the builder's Railpack (deploy/images/pyo.Dockerfile) to
# /usr/local/bin, for pyo-builder's tests in CI. The builder's tests
# check this pin matches the Dockerfile's.
set -euo pipefail
url=https://github.com/railwayapp/railpack/releases/download/v0.43.1/railpack-v0.43.1-x86_64-unknown-linux-musl.tar.gz
sha256=7a3849a68b2f142dcd9cfae68c91c5d5cb3bab4b1e749fc796cd9190a3811ea0
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/railpack.tgz" "$url"
echo "$sha256  $tmp/railpack.tgz" | sha256sum -c -
mkdir "$tmp/rp" && tar -xzf "$tmp/railpack.tgz" -C "$tmp/rp"
sudo install -m 0755 "$(find "$tmp/rp" -type f -name railpack | head -n1)" /usr/local/bin/railpack
railpack --version
