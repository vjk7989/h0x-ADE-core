#!/usr/bin/env bash
# Installs the latest released HyperOx trio (h0x-core, h0x-ade, h0x) the way a user
# would, into $E2E_ROOT/bin, and writes $E2E_ROOT/paths.env (HC=, HX=, HRT=).
# Needs: E2E_ROOT (an absolute, writable folder), curl, tar; minisign on Linux/macOS.
# h0x-core: its own installer (it checks the release signature itself).
# h0x-ade, h0x: the release asset plus its .minisig, checked against the release key
# (the same key and the same `file:` binding the installers and updaters use).
set -euo pipefail

root=${E2E_ROOT:?E2E_ROOT is not set}
KEY='RWTEdqtOBn6VKQ/x9x3qWk/gIWUb/I1OlZXsTlHyrkp9Yq30qBki+tdt'
core_rel=https://github.com/vjk7989/h0x-ADE-core/releases/latest/download
app_rel=https://github.com/vjk7989/h0x-ADE-app/releases/latest/download
rt_rel=https://github.com/vjk7989/h0x-ADE-Runtime/releases/latest/download

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  MINGW* | MSYS* | CYGWIN*) os=windows ;;
  *) echo "no build for $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) echo "no build for $(uname -m)" >&2; exit 1 ;;
esac
# Runners: macOS is Apple Silicon, Windows and Linux are x64.
target=$os-$arch

native() { if [ "$os" = windows ]; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
fetch() { curl -fsSL --retry 3 --proto '=https' --proto-redir '=https' "$1" -o "$2"; }

mkdir -p "$root/bin/core" "$root/bin/app" "$root/bin/runtime" "$root/dl"

echo "== h0x-core"
if [ "$os" = windows ]; then
  H0X_CORE_INSTALL_DIR=$(native "$root/bin/core") pwsh -NoProfile -Command "irm $core_rel/install.ps1 | iex"
  hc=$root/bin/core/h0x-core.exe
else
  fetch "$core_rel/install.sh" "$root/dl/core-install.sh"
  H0X_CORE_INSTALL_DIR=$root/bin/core sh "$root/dl/core-install.sh"
  hc=$root/bin/core/h0x-core
fi
[ -x "$hc" ] || { echo "h0x-core did not install" >&2; exit 1; }

# Downloads $2 from $1 (a releases/latest/download URL) with its .minisig and verifies it.
# Archives go to the installed h0x-core, which checks them against the release key;
# a bare binary goes to minisign. Both bind the signature to the asset's own name.
get_verified() {
  local rel=$1 asset=$2 out=$root/dl/$2
  fetch "$rel/$asset" "$out"
  fetch "$rel/$asset.minisig" "$out.minisig"
  case $asset in
    *.tar.gz | *.zip)
      "$hc" update --from "$(native "$out")" --check >/dev/null
      ;;
    *)
      local said
      said=$(minisign -Vm "$out" -x "$out.minisig" -P "$KEY" 2>&1) || { echo "$said" >&2; return 1; }
      printf '%s\n' "$said" | tr '\t' '\n' | grep -qx "file:$asset" || { echo "signature is for another file: $said" >&2; return 1; }
      ;;
  esac
  echo "verified $asset"
}

echo "== h0x-ade (app)"
get_verified "$app_rel" "h0x-ade-$target.tar.gz"
if [ "$os" = windows ]; then
  # Windows' own tar: Git Bash's reads `D:` as a remote host.
  /c/Windows/System32/tar.exe -xzf "$(native "$root/dl/h0x-ade-$target.tar.gz")" -C "$(native "$root/bin/app")"
else
  tar -xzf "$root/dl/h0x-ade-$target.tar.gz" -C "$root/bin/app"
fi
case $os in
  linux) hx=$root/bin/app/h0x-ade/h0x-ade ;;
  macos) hx=$root/bin/app/HyperOx.app/Contents/MacOS/HyperOx ;;
  windows) hx=$root/bin/app/h0x-ade/h0x-ade.exe ;;
esac
[ -x "$hx" ] || { echo "the app archive holds no $hx" >&2; exit 1; }

echo "== h0x (runtime)"
if [ "$os" = windows ]; then
  get_verified "$rt_rel" "h0x-$target.zip"
  # Windows' own tar opens zips; Git Bash's cannot.
  /c/Windows/System32/tar.exe -xf "$(native "$root/dl/h0x-$target.zip")" -C "$(native "$root/bin/runtime")"
  hrt=$(find "$root/bin/runtime" -name h0x.exe | head -n 1)
else
  get_verified "$rt_rel" "h0x-$target"
  cp "$root/dl/h0x-$target" "$root/bin/runtime/h0x"
  chmod 755 "$root/bin/runtime/h0x"
  hrt=$root/bin/runtime/h0x
fi
[ -n "${hrt:-}" ] && [ -x "$hrt" ] || { echo "the runtime download holds no h0x" >&2; exit 1; }

printf 'HC=%s\nHX=%s\nHRT=%s\n' "$hc" "$hx" "$hrt" > "$root/paths.env"
echo "installed: $hc / $hx / $hrt"
