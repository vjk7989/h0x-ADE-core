# Shipped notices

`just notices` combines these texts with every registry package in `Cargo.lock`, including target-specific and build dependencies. It reads full LICENSE/COPYING/NOTICE files from Cargo's downloaded sources and fails if none exist. It needs Cargo, jq, and Unix tools. The result is embedded in `h0x-core --licenses` and shipped in release archives.

Non-Cargo inputs are checked in so regeneration doesn't depend on a license server:

- `ghostty.txt`: Ghostty's MIT license at submodule commit `1e5cdb865807abc990181efb8ebfa9122955cce2`.
- `simdutf.txt`, `highway.txt`, `wuffs.txt`: the libraries built into libghostty-vt, at the versions pinned by Ghostty.
- `zig.txt`: Zig 0.16.0 compiler_rt.
- `uucode*.txt`: [uucode at 9d55524551411b493cca41ca06363625d90aff1e](https://github.com/jacobsandlund/uucode/tree/9d55524551411b493cca41ca06363625d90aff1e), its `LICENSE.md` and both files in `licenses/`. Include the Hoehrmann decoder notice conservatively alongside the Unicode tables; both are part of the imported module.
- `musl.txt`: [musl v1.2.5 COPYRIGHT](https://github.com/ifduyue/musl/blob/v1.2.5/COPYRIGHT). Rust 1.95.0's `src/ci/docker/scripts/musl.sh` and `musl-toolchain.sh` select 1.2.5 with security patches for the release Linux targets. The patches do not replace its license.
- `rust.txt`: Rust 1.95.0's installed `share/doc/rust/COPYRIGHT-library.html`, converted to plain text, followed by `library/compiler-builtins/LICENSE.txt` from the same toolchain's rust-src. These cover the standard library and compiler runtime, which Cargo.lock does not list.
- `qrcodegen.txt`: qrcodegen 1.8.0 (Project Nayuki, MIT) ships no LICENSE file; this is the MIT notice from the top of its `src/lib.rs`. `scripts/notices` uses it for that crate. Refresh it when qrcodegen changes.
- `r-efi.txt`: r-efi (MIT OR Apache-2.0 OR LGPL-2.1-or-later) ships no license file in its crate; this is the MIT notice, chosen from its license expression. It is only used on UEFI targets. `scripts/notices` uses it for that crate.
- `sqlite.txt`: SQLite's public-domain dedication (the blessing at the top of `sqlite3.c`), for the SQLite 3.54.1 amalgamation that `libsqlite3-sys` 0.39.1 compiles in with rusqlite's `bundled` feature. Its crate's LICENSE covers only the Rust bindings. Refresh the version when libsqlite3-sys changes.
- `noble.txt`: the MIT license of paulmillr's `@noble/curves`, `@noble/hashes` and `@noble/ciphers` 2.4.0 (the ciphers copy adds a Thomas Pornin line), bundled into `crates/h0x-core/src/bridge/web/noble.js` for web panel pages without `crypto.subtle`. The rebuild command is in that file's header. Refresh the versions when the bundle changes.

Windows archives also carry Microsoft's ConPTY LICENSE and NOTICE from `distribution/windows/`, checked against the pinned package by `scripts/fetch-conpty.ps1`.

Core ships no GUI bundle, icons, fonts, artwork, or sounds. Ghostty's GUI assets and other GUI dependencies (including fonts) are not part of libghostty-vt's build. QuickJS, taffy, Lucide, Feather, and LobeHub are not used by core.

When changing Ghostty, Rust, Zig, or release targets, review this list and refresh the affected texts before running `just notices`. Cargo dependency additions without notices fail the license test, as do changed or missing checked-in non-Cargo texts. This is a coverage check, not automatic discovery of newly linked native libraries.
