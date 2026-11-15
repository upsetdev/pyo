#!/bin/sh
# Installs the Pyo CLI, or updates it to the latest release:
#
#   curl -fsSL https://pyo.dev/cli.sh | sh
#
# It downloads the CLI for this computer (pyo-linux-x86_64,
# pyo-linux-aarch64, pyo-macos-aarch64 or pyo-macos-x86_64) from Pyo's
# latest release on GitHub and checks it against the release's sha256. Set
# PYO_VERSION=x.y.z for a release other than the latest. On Windows,
# download pyo-windows-x86_64.exe from the release instead.
#
# It installs to /usr/local/bin when you can write there, and ~/.local/bin
# otherwise. Set PYO_INSTALL_DIR for another folder. It never uses sudo.
# Once installed, `pyo upgrade` updates it.
#
# Everything is in functions and only the last line runs anything, so a
# download that stops halfway does nothing.
set -eu

RELEASES="https://github.com/upsetdev/pyo/releases"

fail() {
  echo "pyo: $*" >&2
  exit 1
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    fail "sha256sum is needed to check the download."
  fi
}

# The release's binary for this computer: pyo-<os>-<arch>, the names
# `pyo upgrade` uses too (crates/pyo-cli/src/upgrade.rs).
asset_for() {
  case "$1" in
    Linux) o=linux ;;
    Darwin) o=macos ;;
    *) o= ;;
  esac
  case "$2" in
    x86_64 | amd64) a=x86_64 ;;
    aarch64 | arm64) a=aarch64 ;;
    *) a= ;;
  esac
  [ -n "$o" ] && [ -n "$a" ] && echo "pyo-$o-$a"
}

main() {
  os=$(uname -s)
  arch=$(uname -m)
  # An Intel shell under Rosetta says x86_64 on an Apple silicon Mac: the
  # Mac's own CLI runs better.
  if [ "$os" = Darwin ] && [ "$arch" = x86_64 ] \
    && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
    arch=arm64
  fi
  ASSET=$(asset_for "$os" "$arch") || {
    echo "pyo: there's no CLI release for $os $arch. It runs on Linux and macOS (x86-64 and Arm) and Windows (x86-64)." >&2
    echo "Build it from the Pyo source instead: cargo build --release -p pyo-cli" >&2
    exit 1
  }
  command -v curl >/dev/null 2>&1 || fail "curl is needed to download the CLI."

  version="${PYO_VERSION:-}"
  version="${version#v}"
  if [ -n "$version" ]; then
    base="$RELEASES/download/v$version"
  else
    base="$RELEASES/latest/download"
  fi

  dir="${PYO_INSTALL_DIR:-}"
  if [ -z "$dir" ]; then
    if [ -w /usr/local/bin ]; then dir=/usr/local/bin; else dir="$HOME/.local/bin"; fi
  fi
  mkdir -p "$dir" || fail "can't make $dir. Set PYO_INSTALL_DIR to a folder you can write to."
  [ -w "$dir" ] || fail "can't write to $dir. Set PYO_INSTALL_DIR to a folder you can write to."

  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  curl -fsSL "$base/$ASSET" -o "$work/pyo" \
    || fail "couldn't download $base/$ASSET"
  curl -fsSL "$base/$ASSET.sha256" -o "$work/pyo.sha256" \
    || fail "couldn't download $base/$ASSET.sha256"
  want=$(cut -d' ' -f1 <"$work/pyo.sha256")
  have=$(sha256_of "$work/pyo")
  [ -n "$want" ] && [ "$want" = "$have" ] \
    || fail "the download doesn't match its sha256. Nothing was installed. Try again."

  chmod 755 "$work/pyo"
  # macOS keeps apps from the internet from opening until you allow them.
  # curl doesn't mark its downloads, but clear the mark in case.
  if [ "$os" = Darwin ]; then
    xattr -d com.apple.quarantine "$work/pyo" 2>/dev/null || true
  fi
  # Beside the old one first, then renamed over it: never half a binary.
  cp "$work/pyo" "$dir/.pyo.new"
  mv -f "$dir/.pyo.new" "$dir/pyo"

  echo "Installed $dir/pyo ($("$dir/pyo" --version))"
  case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "Add $dir to your PATH: export PATH=\"$dir:\$PATH\"" ;;
  esac
  echo "Next: pyo login --url https://your-pyo"
}

main "$@"
