#!/usr/bin/env bash
# h0x-wt/rebrand.sh [--check]
# Applies the HyperOx rebrand of worktrunk 0.81.0 (see PATCHES.md) to this folder, in place. Idempotent.
# --check: apply to a scratch copy and fail if the real tree would change.
# Re-vendor: copy upstream src/ templates/ dev/ build.rs Cargo.toml.orig(as Cargo.toml) LICENSE, then run this.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
PARTS=(.cargo src templates dev build.rs Cargo.toml rust-toolchain.toml)

apply() {
  local r=$1
  cd "$r"
  rm -f src/git_wt.rs
  # insta derives snapshot file names from the crate name
  while IFS= read -r f; do
    b=$(basename "$f"); d=$(dirname "$f")
    case "$b" in worktrunk__*) n="h0x_wt__${b#worktrunk__}" ;; wt__*) n="h0x_wt__${b#wt__}" ;; *) continue ;; esac
    [ -e "$d/$n" ] && { echo "snapshot name clash: $d/$n" >&2; exit 1; }
    mv "$f" "$d/$n"
  done < <(find src -name '*.snap')
  mapfile -t RS < <(find src build.rs -name "*.rs")
  mapfile -t ALL < <(find src templates dev build.rs -type f)
  mapfile -t CLI < <(find src/cli -name "*.rs")

  # 1 manifest: package, lib, bin, dropped targets and dev-deps, MSRV and toolchain from the probe
  perl -0777 -pi -e '
    s/\A(?:\[workspace\].*?)?(?=\[package\])/[workspace]\n\n/s;
    s/name = "worktrunk"\nversion/name = "h0x-wt"\nversion/;
    s/default-run = "wt"/default-run = "h0x-wt"/;
    s/\[lib\]\nname = "worktrunk"/[lib]\nname = "h0x_wt"/;
    s/\[\[bin\]\]\nname = "wt"/[[bin]]\nname = "h0x-wt"/;
    s/# Also install as git-wt.*?(?=\[dependencies\])//s;
    s/\[\[bench\]\]\nname = "alias".*?(?=\[lints\.rust\])//s;
    s/criterion = "0\.8"\n//; s/wt-perf = \{ path = "tests\/helpers\/wt-perf" \}\n//;
    s/# Install git-wt binary[^\n]*\ngit-wt = \[\]\n//;
    s/^rust-version = "1\.98"/rust-version = "1.96"/m;
  ' Cargo.toml
  sed -i 's/^channel = "1.98.0"/channel = "1.96.1"/' rust-toolchain.toml
  # crate paths (bin and lib are separate crates)
  perl -pi -e 's/\bworktrunk::/h0x_wt::/g' "${RS[@]}"

  # 2 command name: argv0 fallbacks, clap name
  perl -pi -e 's/unwrap_or_else\(\|\| "wt"\.to_string\(\)\)/unwrap_or_else(|| "h0x-wt".to_string())/; s/falling back to "wt"\./falling back to "h0x-wt"./' src/invocation.rs src/completion.rs
  perl -pi -e 's/DEFAULT_COMMAND_NAME: &str = "wt"/DEFAULT_COMMAND_NAME: &str = "h0x-wt"/; s/#\[command\(name = "wt"\)\]/#[command(name = "h0x-wt")]/' src/cli/mod.rs

  # 3 config folder (user, system, docs)
  perl -pi -e 's/join\("worktrunk"\)/join("h0x-wt")/g; s{\.config/worktrunk}{.config/h0x-wt}g; s{([/\\])worktrunk([/\\]config\.toml)}{$1h0x-wt$2}g; s{(Application Support/)worktrunk}{$1h0x-wt}g' "${ALL[@]}"

  # test expectations that name the old folder or owner
  perl -pi -e 's{(?<=")worktrunk([/\\]config\.toml)}{h0x-wt$1}g; s{max-sixty/myrepo}{vjk7989/myrepo}g' src/config/user/tests.rs

  # agent plugin file names (installers are disabled, see PATCHES.md)
  perl -pi -e 's/\bworktrunk\.ts\b/h0x-wt.ts/g' "${ALL[@]}"

  # 4 env prefix (WORKTRUNK_ and WORKTRUNK__A__B overrides)
  perl -pi -e 's/WORKTRUNK_/H0X_WT_/g' "${ALL[@]}"

  # 5 git config keys
  perl -pi -e 's/\bworktrunk\.(?=(?:default|history|state|hints)\b)/h0x-wt./g; s/\^worktrunk\\\./^h0x-wt\\./g;' "${ALL[@]}"

  perl -pi -e 's/add_filter\(r"wt-\[0-9a-f\]\+", "wt"\)/add_filter(r"h0x_wt-[0-9a-f]+", "h0x-wt")/' src/output/shell_integration.rs
  # 6 repo state folder and 7 project config
  perl -pi -e 's{\.git/wt/}{.git/h0x-wt/}g; s{<git-common-dir>/wt/}{<git-common-dir>/h0x-wt/}g; s{git_common_dir\(\)\.join\("wt"\)}{git_common_dir().join("h0x-wt")}g; s{(?<![\w-])wt\.toml}{h0x-wt.toml}g' "${ALL[@]}"

  # 9 links
  perl -pi -e 's{max-sixty/worktrunk}{vjk7989/h0x-ADE-core}g' "${ALL[@]}"

  # 10 version text, help examples
  perl -pi -e 's/<(underline|bold|dim|cyan|green|red)>wt\b/<$1>h0x-wt/g' "${ALL[@]}"
  perl -0777 -pi -e 's/(VERSION\.get_or_init\(\|\| \{.*?)resolve_version\(\s*option_env!\("VERGEN_GIT_DESCRIBE"\),\s*env!\("CARGO_PKG_VERSION"\),?\s*\)/$1format!(\n            "{} (HyperOx Worktrees, from worktrunk 0.81.0)",\n            resolve_version(None, env!("CARGO_PKG_VERSION"))\n        )/s' src/cli/mod.rs
  perl -pi -e 's/`wt( |`)/`h0x-wt$1/g; s/\$ wt /\$ h0x-wt /g; s/^(\s*)wt (?=[a-z-])/$1h0x-wt /' "${CLI[@]}"
  perl -pi -e 's/`wt( |`)/`h0x-wt$1/g; s/\$ wt /\$ h0x-wt /g' src/md_help.rs
}

if [ "${1:-}" = "--check" ]; then
  T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
  for p in "${PARTS[@]}"; do cp -r "$HERE/$p" "$T/"; done
  apply "$T"
  rc=0
  for p in "${PARTS[@]}"; do diff -r "$HERE/$p" "$T/$p" >/dev/null || { echo "rebrand.sh --check: $p would change" >&2; rc=1; }; done
  exit $rc
fi
apply "$HERE"
