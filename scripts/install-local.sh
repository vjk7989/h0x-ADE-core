#!/bin/sh
# Builds a release binary into ~/.local/bin (replacing any older one), then
# restarts the core the planned way so running terminals pick up the new build.
set -eu
cd "$(dirname "$0")/.."
cargo build --release
bin="${HC_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$bin"
install -m 755 target/release/herdr-core "$bin/herdr-core.new"
mv -f "$bin/herdr-core.new" "$bin/herdr-core"
[ -e "$bin/hc" ] || [ -L "$bin/hc" ] || ln -s herdr-core "$bin/hc"
"$bin/herdr-core" server restart
