#!/bin/sh
# Installs the Pyo CLI from this Pyo:
#   curl -fsSL __PYO_URL__/install/cli.sh | sh
# Into /usr/local/bin when it's writable, else ~/.local/bin; or set
# PYO_INSTALL_DIR.
set -eu
URL="__PYO_URL__"

os=$(uname -s)
arch=$(uname -m)
if [ "$os" != "Linux" ] || [ "$arch" != "x86_64" ]; then
  echo "This Pyo has the CLI for Linux x86-64 only ($os $arch here)." >&2
  echo "Build it from the Pyo source instead: cargo build --release -p pyo-cli" >&2
  exit 1
fi

dir="${PYO_INSTALL_DIR:-}"
if [ -z "$dir" ]; then
  if [ -w /usr/local/bin ]; then dir=/usr/local/bin; else dir="$HOME/.local/bin"; fi
fi
mkdir -p "$dir"
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
curl -fsSL "$URL/install/pyo" -o "$tmp"
chmod 755 "$tmp"
mv "$tmp" "$dir/pyo"
trap - EXIT

echo "Installed $dir/pyo ($("$dir/pyo" --version))"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH." ;;
esac
echo "Next: pyo login --url $URL"
