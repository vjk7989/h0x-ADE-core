#!/bin/sh
# Builds a previous release (a git ref) and checks that updating from it to
# this tree keeps every terminal and machine running: tests/upgrade.rs.
# Usage: scripts/upgrade-test.sh <ref>   (CI: the last release tag)
set -eu
ref=${1:?usage: scripts/upgrade-test.sh <git ref of the previous release>}
root=$(git rev-parse --show-toplevel)
sha=$(git -C "$root" rev-parse --short "$ref")
bin=/var/tmp/h0x-core-upgrade/$sha/h0x-core
if [ ! -x "$bin" ]; then
    src=/var/tmp/h0x-core-upgrade/$sha-src
    rm -rf "$src"
    git -C "$root" worktree add --detach "$src" "$ref"
    # Ghostty: this checkout's when the ref pins the same commit, else fetched.
    if [ "$(git -C "$root" ls-tree "$ref" vendor/ghostty)" = "$(git -C "$root" ls-tree HEAD vendor/ghostty)" ]; then
        rmdir "$src/vendor/ghostty" && ln -s "$root/vendor/ghostty" "$src/vendor/ghostty"
    else
        git -C "$src" submodule update --init --depth 1 vendor/ghostty
    fi
    # The release may need an older Zig than this checkout (ZIG_OLD, set by CI).
    (cd "$src" && ZIG="${ZIG_OLD:-${ZIG:-zig}}" CARGO_TARGET_DIR=/var/tmp/h0x-core-upgrade/target cargo build -q --bin h0x-core)
    mkdir -p "$(dirname "$bin")"
    cp /var/tmp/h0x-core-upgrade/target/debug/h0x-core "$bin"
    git -C "$root" worktree remove --force "$src"
fi
cd "$root"
H0X_CORE_OLD_BIN="$bin" cargo test -q --test upgrade -- --nocapture
