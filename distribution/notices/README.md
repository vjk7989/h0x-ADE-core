# Shipped notices

`just notices` combines these texts with every registry package in `Cargo.lock`, including target-specific and build dependencies. It reads full LICENSE/COPYING/NOTICE files from Cargo's downloaded sources and fails if none exist. It needs Cargo, jq, and Unix tools. The result is embedded in `h0x-core --licenses` and shipped in release archives.

Non-Cargo inputs are checked in so regeneration doesn't depend on a license server:

- `ghostty.txt`: Ghostty's MIT license at submodule commit `83fb95624b0439ee660c2679b43b38c8b63769cd` (v1.4.0).
- `simdutf.txt`, `highway.txt`, `wuffs.txt`: the libraries built into libghostty-vt, at the versions pinned by Ghostty.
- `zig.txt`: Zig 0.17.0 compiler_rt.
- `uucode*.txt`: [uucode at ea62149739404a73c202b48a33bf6dd2af4bd9b0](https://github.com/jacobsandlund/uucode/tree/ea62149739404a73c202b48a33bf6dd2af4bd9b0), its `LICENSE.md` and both files in `licenses/`. Include the Hoehrmann decoder notice conservatively alongside the Unicode tables; both are part of the imported module.
- `musl.txt`: [musl v1.2.5 COPYRIGHT](https://github.com/ifduyue/musl/blob/v1.2.5/COPYRIGHT). Rust 1.95.0's `src/ci/docker/scripts/musl.sh` and `musl-toolchain.sh` select 1.2.5 with security patches for the release Linux targets. The patches do not replace its license.
- `rust.txt`: Rust 1.95.0's installed `share/doc/rust/COPYRIGHT-library.html`, converted to plain text, followed by `library/compiler-builtins/LICENSE.txt` from the same toolchain's rust-src. These cover the standard library and compiler runtime, which Cargo.lock does not list.
- `qrcodegen.txt`: qrcodegen 1.8.0 (Project Nayuki, MIT) ships no LICENSE file; this is the MIT notice from the top of its `src/lib.rs`. `scripts/notices` uses it for that crate. Refresh it when qrcodegen changes.
- `r-efi.txt`: r-efi (MIT OR Apache-2.0 OR LGPL-2.1-or-later) ships no license file in its crate; this is the MIT notice, chosen from its license expression. It is only used on UEFI targets. `scripts/notices` uses it for that crate.
- `assert_enum_variants.txt`: assert_enum_variants (MIT OR Apache-2.0), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `clipboard-win.txt`: clipboard-win (BSL-1.0), a dependency of h0x-wt, ships no license file in its crate; this is the Boost Software License text. `scripts/notices` uses it for that crate.
- `crokey-proc_macros.txt`: crokey-proc_macros (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `dispatch2.txt`: dispatch2 (Zlib OR Apache-2.0 OR MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `jsonschema-regex.txt`: jsonschema-regex (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `jsonschema-value.txt`: jsonschema-value (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `lazy-regex-proc_macros.txt`: lazy-regex-proc_macros (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `objc2.txt`: objc2 (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `objc2-app-kit.txt`: objc2-app-kit (Zlib OR Apache-2.0 OR MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `objc2-core-foundation.txt`: objc2-core-foundation (Zlib OR Apache-2.0 OR MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `objc2-core-graphics.txt`: objc2-core-graphics (Zlib OR Apache-2.0 OR MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `objc2-encode.txt`: objc2-encode (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `objc2-foundation.txt`: objc2-foundation (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `objc2-io-surface.txt`: objc2-io-surface (Zlib OR Apache-2.0 OR MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `osc8.txt`: osc8 (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `petname-macros.txt`: petname-macros (Apache-2.0), a dependency of h0x-wt, ships no license file in its crate; this is the Apache-2.0 text. `scripts/notices` uses it for that crate.
- `ratatui-termina.txt`: ratatui-termina (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `relative-path.txt`: relative-path (MIT OR Apache-2.0), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `sigchld.txt`: sigchld (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `unicode-display-width.txt`: unicode-display-width (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `uuid-simd.txt`: uuid-simd (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `valuable.txt`: valuable (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `vsimd.txt`: vsimd (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `vtparse.txt`: vtparse (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `wasip2.txt`: wasip2 (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `wezterm-input-types.txt`: wezterm-input-types (MIT), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice. `scripts/notices` uses it for that crate.
- `winapi-i686-pc-windows-gnu.txt`: winapi-i686-pc-windows-gnu (MIT/Apache-2.0), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `winapi-x86_64-pc-windows-gnu.txt`: winapi-x86_64-pc-windows-gnu (MIT/Apache-2.0), a dependency of h0x-wt, ships no license file in its crate; this is the MIT notice, chosen from its license expression. `scripts/notices` uses it for that crate.
- `sqlite.txt`: SQLite's public-domain dedication (the blessing at the top of `sqlite3.c`), for the SQLite 3.54.1 amalgamation that `libsqlite3-sys` 0.39.1 compiles in with rusqlite's `bundled` feature. Its crate's LICENSE covers only the Rust bindings. Refresh the version when libsqlite3-sys changes.
- `noble.txt`: the MIT license of paulmillr's `@noble/curves`, `@noble/hashes` and `@noble/ciphers` 2.4.0 (the ciphers copy adds a Thomas Pornin line), bundled into `crates/h0x-core/src/bridge/web/noble.js` for web panel pages without `crypto.subtle`. The rebuild command is in that file's header. Refresh the versions when the bundle changes.
- `worktrunk.txt`: the LICENSE of worktrunk 0.81.0 (Maximilian Roos, MIT OR Apache-2.0, used under MIT), from which HyperOx Worktrees (h0x-wt) is copied. Refresh it when the copy is re-vendored.

Windows archives also carry Microsoft's ConPTY LICENSE and NOTICE from `distribution/windows/`, checked against the pinned package by `scripts/fetch-conpty.ps1`.

Core ships no GUI bundle, icons, fonts, artwork, or sounds. Ghostty's GUI assets and other GUI dependencies (including fonts) are not part of libghostty-vt's build. QuickJS, taffy, Lucide, Feather, and LobeHub are not used by core.

When changing Ghostty, Rust, Zig, or release targets, review this list and refresh the affected texts before running `just notices`. Cargo dependency additions without notices fail the license test, as do changed or missing checked-in non-Cargo texts. This is a coverage check, not automatic discovery of newly linked native libraries.
