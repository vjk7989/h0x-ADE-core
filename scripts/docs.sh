#!/bin/sh
# Render CLI help and check human examples using the binary built by Cargo.
set -eu
case "${1:---check}" in
  --write) export HC_UPDATE_DOCS=1 ;;
  --check) unset HC_UPDATE_DOCS ;;
  *) echo 'usage: sh scripts/docs.sh [--write|--check]' >&2; exit 2 ;;
esac
cargo test --locked -p herdr-core --test cli_docs
