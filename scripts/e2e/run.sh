#!/usr/bin/env bash
# One end-to-end run: install the latest released trio into an isolated home, then
# run the black-box checks against it. Same script on Linux, macOS (bash) and
# Windows (Git Bash), and inside the OpenShell sandbox.
# E2E_ROOT: scratch folder for everything (default: a new temp folder).
# E2E_SKIP_INSTALL=1: use $E2E_ROOT/paths.env as it is (local tests of the script).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=${E2E_ROOT:-$(mktemp -d)}
mkdir -p "$root"
root=$(cd "$root" && pwd)
if command -v cygpath >/dev/null 2>&1; then root=$(cygpath -m "$root"); fi
export E2E_ROOT=$root

h=$root/home
mkdir -p "$h/config" "$h/state" "$h/appdata" "$h/localappdata" "$h/tmp" "$h/run" "$h/app"
export HOME=$h USERPROFILE=$h
export XDG_CONFIG_HOME=$h/config XDG_STATE_HOME=$h/state XDG_DATA_HOME=$h/state XDG_CACHE_HOME=$h/state
export APPDATA=$h/appdata LOCALAPPDATA=$h/localappdata TEMP=$h/tmp TMP=$h/tmp TMPDIR=$h/tmp
# Unix sockets live here (a short path: sockets have a length limit).
export XDG_RUNTIME_DIR=$h/run
chmod 700 "$XDG_RUNTIME_DIR"
export H0X_ADE_HOME=$h/app H0X_CORE_SESSION=e2e H0X_SESSION=e2e
export H0X_CORE_DRAFT_MS=3000

if [ -z "${E2E_SKIP_INSTALL:-}" ]; then bash "$here/install.sh"; fi

set -a
# shellcheck disable=SC1091
. "$root/paths.env"
set +a
# The app looks for h0x-core on PATH; give it the one just installed, never another.
export PATH="$root/bin/core:$PATH"

echo "trio: $("$HC" --version) / $("$HRT" --version) / $("$HX" --version)"
exec bash "$here/blackbox.sh"
