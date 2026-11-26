# h0x-wt patches

Base: worktrunk 0.81.0 (crates.io), copied byte for byte except as listed here.

## Copy (WT-V1)
- Copied: `src/`, `templates/`, `dev/`, `build.rs`, `LICENSE`, `Cargo.lock`, `rust-toolchain.toml`.
  Not copied: `tests/`, `benches/`, `docs/`, `nix/`, `plugins/`, `skills/`, `hooks/`, `src/git_wt.rs`, the changelog.
- `Cargo.toml` comes from upstream `Cargo.toml.orig`: own `[workspace]`; package `worktrunk` -> `h0x-wt`; lib
  `h0x_wt`; bin `h0x-wt` (the `git-wt` bin, `[[test]]`, `[[bench]]`, `criterion`, `wt-perf` and the cargo-affected
  metadata dropped). `Cargo.lock` pruned by cargo once: no version changed, only removals and the package rename.
- `worktrunk::` paths in `src/` and `build.rs` -> `h0x_wt::` (the bin refers to its lib by crate name).

## Toolchain probe (2026-11-26)
- Upstream 0.81.0 declares `rust-version = "1.98"`. With `+1.95.0 check --ignore-rust-version` the whole dependency
  tree and h0x-wt compile; no code patch was needed.
- Declared minimum of the locked tree is 1.96: `vergen`, `vergen-gitcl`, `vergen-lib` 10.0.3 (build dependencies)
  require rustc 1.96.0. Chosen: `rust-version = "1.96"`, `rust-toolchain.toml` channel 1.96.1. CI installs 1.96.1 for
  the h0x-wt steps only; core stays on 1.95.0.

## Rebrand (WT-V2) - `bash rebrand.sh` (in place, idempotent) and `bash rebrand.sh --check`
Rules, in script order: (1) manifest and toolchain as above, `worktrunk::` -> `h0x_wt::`, snapshot files
`worktrunk__*`/`wt__*` -> `h0x_wt__*`; (2) command name `h0x-wt` (clap name, argv0 fallbacks); (3) config folder
`~/.config/h0x-wt/` (user, system, docs); (3b) two test expectations in `config/user/tests.rs`; plugin file names
`worktrunk.ts` -> `h0x-wt.ts`; (4) env prefix `WORKTRUNK_` -> `H0X_WT_` (incl. `H0X_WT__A__B` overrides); (5) git
config keys `worktrunk.{default*,history*,state,hints}` -> `h0x-wt.*`; (6) `.git/wt/` -> `.git/h0x-wt/`;
(7) `wt.toml` -> `h0x-wt.toml`; (9) `max-sixty/worktrunk` -> `vjk7989/h0x-ADE-core`; (10) version text
`h0x-wt 0.81.0 (HyperOx Worktrees, from worktrunk 0.81.0)` (git describe of OUR repo is no longer used), backticked
`wt ` and `$ wt ` and line-start `wt <verb>` in `src/cli/**` and `src/md_help.rs` -> `h0x-wt`; one insta filter in
`output/shell_integration.rs`; styled hint names `<underline>wt` etc. -> `h0x-wt`. 17 rule lines in all (about 40
substitutions).
Also copied: `.cargo/config.toml` (COLUMNS=80 for the unit tests).
Snapshot regeneration (no cargo-insta here): `INSTA_UPDATE=always cargo test --lib` rewrote the file snapshots; the four
inline snapshots (in `md_help.rs`, `output/shell_integration.rs`, `commands/step/prune.rs` and `git/recover.rs`) were rewritten by hand from the
`.pending-snap` output of `INSTA_UPDATE=always cargo test --bins`. After a re-vendor, repeat both.
Known bin-test failure: `commands::configure_shell::tests::test_aur_pkgbuild_ships_the_current_fish_completion` reads
upstream's `.github/aur/PKGBUILD`, which is not copied. GATE-WT runs `--lib` only.

## Disabled global installers (WT-V3)
Each exits 1 with "`<command>` is not available in HyperOx Worktrees" and touches nothing:
- `src/main.rs`: `config shell install|uninstall`; `config plugins claude|codex|omp|opencode|pi install|uninstall`
  and `claude install-statusline` (new `unavailable()`; the handlers stay in `src/commands/` unreferenced).
- `src/commands/config/show.rs`: `render_version_check` no longer calls GitHub (`config show --full` was the only
  caller); `fetch_latest_version` removed. Test `version_check_is_not_available`; installer test
  `global_installers_are_refused` in `src/main.rs`.

## Notes for callers (found while wiring core, WT-R1)
- `list` takes no `--no-hooks` (it runs none); `merge` has no `--squash` (squash is its default; `--no-squash` turns it
  off) and removes the worktree in the background unless `--no-remove`, so core merges with `--no-remove` and removes
  with `remove <path> --foreground` from the main checkout.
- A conflicting merge leaves a rebase half done in the worktree; core aborts it (`git rebase --abort`) and reports the files.
- `H0X_WT__WORKTREE_PATH` overrides `worktree-path`; core sets it for `switch --create` only.
- Squash commits need a git identity (user.name/user.email), as any commit does.
