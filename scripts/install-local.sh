#!/bin/sh
# Builds a release binary into ~/.local/bin (replacing any older one), then
# restarts the core the planned way so running terminals pick up the new build.
set -eu
cd "$(dirname "$0")/.."
cargo build --release
bin="${H0X_CORE_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$bin"
install -m 755 target/release/h0x-core "$bin/h0x-core.new"
mv -f "$bin/h0x-core.new" "$bin/h0x-core"
"$bin/h0x-core" server restart
