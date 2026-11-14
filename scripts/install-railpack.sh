#!/usr/bin/env bash
# Installs the builder's Railpack (deploy/images/pyo.Dockerfile) to
# /usr/local/bin, for pyo-builder's tests in CI. The builder's tests
# check this pin matches the Dockerfile's.
set -euo pipefail
url=https://github.com/railwayapp/railpack/releases/download/v0.40.1/railpack-v0.40.1-x86_64-unknown-linux-musl.tar.gz
sha256=2842de93e68713af9037e0bc0a398d7da78f3b96aa4804303a638db2bc69bd30
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/railpack.tgz" "$url"
echo "$sha256  $tmp/railpack.tgz" | sha256sum -c -
mkdir "$tmp/rp" && tar -xzf "$tmp/railpack.tgz" -C "$tmp/rp"
sudo install -m 0755 "$(find "$tmp/rp" -type f -name railpack | head -n1)" /usr/local/bin/railpack
railpack --version
