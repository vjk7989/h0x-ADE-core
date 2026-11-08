#!/bin/sh
# Installs h0x-core, or updates it:
#
#   curl -fsSL https://github.com/vjk7989/h0x-ADE-core/releases/latest/download/install.sh | sh
#
# Into ~/.local/bin/h0x-core. An h0x-core that is already there is swapped
# the planned way (h0x-core update --from): running terminals keep running.
# The download must carry the release key's signature (its .minisig).
# H0X_CORE_VERSION=0.2.0 picks a version; H0X_CORE_RELEASES_URL points
# elsewhere (tests); H0X_CORE_INSTALL_DIR installs elsewhere.
set -eu

releases=${H0X_CORE_RELEASES_URL:-https://github.com/vjk7989/h0x-ADE-core/releases}
bin=${H0X_CORE_INSTALL_DIR:-$HOME/.local/bin}

say() { printf '%s\n' "$*"; }
fail() { say "h0x-core: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "needs $1"; }
need curl
need tar

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) fail "h0x-core runs on Linux and macOS for now" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) fail "no build for $(uname -m)" ;;
esac
target="$os-$arch"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# https only, and the download on GitHub; a site set by H0X_CORE_RELEASES_URL
# (tests: a local http one) is trusted as it is.
proto='=https'
[ -n "${H0X_CORE_RELEASES_URL:-}" ] && proto='=https,http'

manifest="$releases/latest/download/latest.json"
[ -n "${H0X_CORE_VERSION:-}" ] && manifest="$releases/download/v$H0X_CORE_VERSION/manifest.json"
curl -fsSL --proto "$proto" --proto-redir "$proto" "$manifest" -o "$tmp/manifest.json" || fail "cannot reach $manifest"
# Read without jq: the manifest is flat enough for sed once spaces are gone.
flat=$(tr -d '\n ' < "$tmp/manifest.json")
entry=$(printf '%s' "$flat" | sed -n "s/.*\"$target\":{\([^}]*\)}.*/\1/p")
url=$(printf '%s' "$entry" | sed -n 's/.*"url":"\([^"]*\)".*/\1/p')
want=$(printf '%s' "$entry" | sed -n 's/.*"sha256":"\([^"]*\)".*/\1/p')
version=$(printf '%s' "$flat" | sed -n 's/^{"version":"\([^"]*\)".*/\1/p')
[ -n "$url" ] && [ -n "$want" ] || fail "no build for $target in $manifest"

if [ -z "${H0X_CORE_RELEASES_URL:-}" ]; then
  case $url in
    https://github.com/* | https://objects.githubusercontent.com/* | https://release-assets.githubusercontent.com/*) ;;
    *) fail "refusing to download $url: not an https link on GitHub" ;;
  esac
fi
# Fetched exactly as the list gives it (a mirror's signed URL included),
# under the release's own file name: its signature is bound to that name.
asset=${url##*/}
asset=${asset%%[?#]*}
archive="$tmp/$asset"
curl -fsSL --proto "$proto" --proto-redir "$proto" "$url" -o "$archive" || fail "download failed: $url"
[ "$(sha256 "$archive")" = "$want" ] || fail "the download is damaged (checksum mismatch); try again"
tar -xzf "$archive" -C "$tmp"
"$tmp/h0x-core" --version >/dev/null 2>&1 || fail "the downloaded h0x-core does not run on this machine"
# The checksum comes from the same place as the file: the signature is what
# proves the release key made it. The downloaded h0x-core checks it with its
# embedded key before anything is installed (it has to be trusted to say so:
# that first step rests on TLS to GitHub; every later update is checked by the
# h0x-core already installed). A site set by H0X_CORE_RELEASES_URL (tests) is not signed.
if [ -z "${H0X_CORE_RELEASES_URL:-}" ]; then
  curl -fsSL --proto "$proto" --proto-redir "$proto" "${url%%[?#]*}.minisig" -o "$archive.minisig" || fail "no signature for $asset: ${url%%[?#]*}.minisig"
  "$tmp/h0x-core" update --from "$archive" --check >/dev/null || fail "$asset is not signed by the h0x-core release key (see above)"
fi
mkdir -p "$bin"

if [ -x "$bin/h0x-core" ]; then
  # Swapped the planned way, so its terminals keep running.
  if [ -n "${H0X_CORE_RELEASES_URL:-}" ]; then from=$tmp/h0x-core; else from=$archive; fi
  "$bin/h0x-core" update --from "$from" >/dev/null || fail "updating $bin/h0x-core failed (see above)"
  say "h0x-core updated to $version."
else
  cp "$tmp/h0x-core" "$bin/h0x-core.new"
  chmod 755 "$bin/h0x-core.new"
  mv -f "$bin/h0x-core.new" "$bin/h0x-core"
  say "h0x-core $version installed into $bin/h0x-core."
fi

case ":$PATH:" in
  *":$bin:"*) ;;
  *) say "Add $bin to your PATH to use h0x-core." ;;
esac
say "Agents: h0x-core --skill. People: h0x-core --help."
