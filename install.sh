#!/bin/sh
# Install Sonde on Linux or macOS:
#
#   curl -fsSL https://raw.githubusercontent.com/morisam1/sonde/master/install.sh | sh
#
# Settings, as environment variables:
#   SONDE_CHANNEL=nightly   install the nightly build instead of the latest release
#   SONDE_VERSION=0.2.0     install this release instead of the latest
#
# Linux: the program goes in ~/.local/bin, with a launcher in the app menu.
# macOS: Sonde.app goes in /Applications, or ~/Applications when that is
# not writable. Nothing needs root, and every download is checked against
# its published SHA-256 checksum before it is installed.
set -eu

repo="morisam1/sonde"
base="${SONDE_DOWNLOAD_BASE:-https://github.com/$repo/releases}"
channel="${SONDE_CHANNEL:-stable}"
version="${SONDE_VERSION:-}"

say() { printf 'sonde: %s\n' "$*"; }
fail() { printf 'sonde: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) fail "this script installs on Linux and macOS. On Windows, see the README for install.ps1." ;;
esac
arch=$(uname -m)
case "$os/$arch" in
    linux/x86_64 | linux/amd64) package="sonde-x86_64-unknown-linux-gnu.tar.gz" ;;
    linux/*) fail "there is no Linux build for $arch yet. Build from source: https://github.com/$repo#build-from-source" ;;
    macos/*) package="sonde-universal-apple-darwin.zip" ;;
esac

if [ -n "$version" ]; then
    url="$base/download/v${version#v}"
elif [ "$channel" = nightly ]; then
    url="$base/download/nightly"
elif [ "$channel" = stable ]; then
    url="$base/latest/download"
else
    fail "SONDE_CHANNEL must be stable or nightly, not $channel"
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    fail "curl or wget is needed to download Sonde"
fi
if command -v sha256sum >/dev/null 2>&1; then
    digest() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    digest() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    fail "sha256sum or shasum is needed to check the download"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

say "downloading $package"
fetch "$url/$package" "$work/$package" || fail "could not download $url/$package"
fetch "$url/$package.sha256" "$work/$package.sha256" || fail "could not download the checksum"
expected=$(cut -d' ' -f1 <"$work/$package.sha256")
actual=$(digest "$work/$package")
[ "$expected" = "$actual" ] || fail "the download does not match its checksum; nothing was installed"

if [ "$os" = linux ]; then
    tar -xzf "$work/$package" -C "$work"
    sh "$work/sonde/install.sh"
else
    ditto -x -k "$work/$package" "$work/unpacked"
    target=/Applications
    [ -w "$target" ] || target="$HOME/Applications"
    mkdir -p "$target"
    rm -rf "$target/Sonde.app"
    mv "$work/unpacked/Sonde.app" "$target/Sonde.app"
    say "installed $target/Sonde.app"
fi
say "done. Start Sonde from your app menu."
