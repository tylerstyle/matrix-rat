# Review: M12 — `release-profile` (`Cargo.toml` release and `release-small` profiles)

**Commit reviewed:** `5f72b84` on `origin/release-profile` (diff against `main` `981def7`: `Cargo.toml` only).  
**Files inspected:** `Cargo.toml` (`main` and `5f72b84`), `Cargo.lock`, `default.nix`, `shell.nix`, `src/main.rs`, `src/terminal.rs`, `src/rich.rs`, `src/matrix/` (`spawn`/`JoinSet` call sites), `~/.cargo/registry/src/.../{aws-lc-sys-0.47.0,blake3-1.8.7,libsqlite3-sys-0.38.2,matrix-sdk-store-encryption-0.19.1,ratatui-image-11.1.0}`, `<nixpkgs>` `cargo-setup-hook.sh` / `aws-lc/package.nix`, and `target/size/release/matrix-rat`.

---

## Verdict

**Approve** the branch strategy: ship **`[profile.release]` (`lto = "fat"`, `codegen-units = 1`, `strip = true`)** as the `v0.1.0` default and keep **`[profile.release-small]`** as opt-in. Before merging (or alongside the cross-check measurements), consider adding a few small missing crypto/image-pipeline siblings (`blake3`, `poly1305`, `chacha20poly1305`, `cbc`, `hkdf`, `base64-simd`, `crc32fast`) to `[profile.release-small.package.*]`.

---

## 1. Risks of `lto = "fat"` + `codegen-units = 1` for this dependency tree

- **`aws-lc-sys` / `ring` / `libsqlite3-sys` / `blake3` (C & assembly):**
  - `ring` (`0.17.14` in `Cargo.lock`) is **not** built on Linux (`cargo tree --edges no-dev -i ring` is empty; Linux `rustls` / `rustls-webpki` uses `aws-lc-rs v1.19.0` -> `aws-lc-sys v0.47.0`).
  - `aws-lc-sys` (`v0.47.0`), `libsqlite3-sys` (`v0.38.2`), and `blake3` (`v1.8.7`) compile their C sources and hand-written `.S`/NASM assembly via `cmake`/`cc` into native ELF object files inside static archives, not LLVM bitcode (`aws-lc-sys`'s `cmake_builder.rs:22-35` also strips `-flto*` from `CFLAGS`). Rust's `lto = "fat"` and `codegen-units = 1` only govern Rust LLVM bitcode across crates, then `ld` links those `.a` objects normally (`readelf -d` on `target/size/release/matrix-rat` confirms only `libgcc_s.so.1`, `libm.so.6`, `libc.so.6` are `NEEDED`).
  - **Good catch in `5f72b84` (`release-small`):** `aws-lc-sys-0.47.0/builder/main.rs:966-978` and `cc_builder.rs:397-404` inspect Cargo's `OPT_LEVEL` and automatically enable `OPENSSL_SMALL` and **disable AVX-512 assembly** (`MY_ASSEMBLER_IS_TOO_OLD_FOR_512AVX=1`) when `OPT_LEVEL` is `"s"` or `"z"`. Explicitly pinning `aws-lc-sys` to `opt-level = 3` in `[profile.release-small]` avoids that trap.
- **Proc-macros and `scripts/check.sh`:**
  - `Cargo` does not apply `lto` to host proc-macro dylibs (`66` crates in `Cargo.lock` use `proc-macro2` / `-derive` / `-macros`), and `[profile.test]` inherits `dev` (not `release`), so `scripts/check.sh` (`cargo test`, `cargo clippy`, `cargo build`) is unaffected.
- **Memory use on a 16 GB CI runner:**
  - Peak RSS comes from LLVM inside `rustc` merging LLVM bitcode across all ~428 active Rust target crates into a single LLVM module before passing one `.o` + native `.a` libs to `ld` (rather than `ld` itself).
  - Because crate compilation finishes before final binary LTO starts, and `Cargo.toml` has **one** binary target and no separate `tests/`, `benches/`, or `examples/` targets, `cargo build --release` / `nix-build` runs only one fat-LTO `rustc` process at a time — comfortable on a 16 GB runner. (Avoid running `cargo test --release` or `--all-targets --release` in smaller memory containers where test + bin LTO links could run concurrently.)
- **Reproducibility with Nix (`default.nix`):**
  - `codegen-units = 1` + `lto = "fat"` eliminates parallel CGU-split scheduling variance during Rust codegen, which **helps** reproducibility in Nix; `strip = true` is already on `main` and matches Nix's standard strip phase (`default.nix` does not set `separateDebugInfo`).
  - Under `default.nix`, `nixpkgs`'s `cargo-setup-hook.sh:94-118` exports `AWS_LC_SYS_USE_SYSTEM=1` and `LIBSQLITE3_SYS_USE_PKG_CONFIG=1`, linking `pkgs.aws-lc` and `pkgs.sqlite` (`pkg-config`) as Nix store shared libraries outside Rust LTO anyway (`pkgs/by-name/aw/aws-lc/package.nix:16`).

---

## 2. `release-small` hot list (`34` package overrides)

### Validity of package names & multi-version checks
- All **34/34** crate names in `[profile.release-small.package.*]` exist in `Cargo.lock` and in the active Linux non-dev `cargo tree` (`[]` missing).
- `cargo check --profile release-small` on `5f72b84` (verified through dependency resolution and initial crate compilation) emitted **zero Cargo profile warnings** (`cargo 1.99.0`).
- Three overridden names have two versions in the Linux dependency graph:
  - `base64`: `0.22.1` (`oauth2`), `0.23.1` (`vodozemac`, `ruma-common`, `matrix-sdk-sqlite`, `matrix-sdk-store-encryption`, `reqwest`, `hyper-util`, `axum`)
  - `miniz_oxide`: `0.8.9` (`png`), `0.9.1` (`flate2`)
  - `sha2`: `0.10.9` (`oauth2`), `0.11.1` (`vodozemac`, `ed25519-dalek`, `hpke`, `matrix-sdk`, `matrix-sdk-crypto`, `matrix-sdk-store-encryption`)
  Unversioned `[profile.release-small.package.<name>]` applies to both versions cleanly without warning.

### Missing CPU-hot siblings worth adding (small crates, direct hot-path coverage)
1. **`blake3`** (`v1.8.7`, `8,450` SLOC + C/ASM via `cc::Build`): `matrix-sdk-store-encryption` calls `blake3::keyed_hash` and `blake3::derive_key` (`matrix-sdk-store-encryption-0.19.1/src/lib.rs:25, 673`) on encrypted SQLite store lookups/writes (always active since local stores are encrypted). Because `blake3`'s `build.rs` reads Cargo's `OPT_LEVEL` for `cc::Build`, omitting `blake3` compiles its C code under `-Os`.
2. **`poly1305`** (`v0.9.1`, `2,880` SLOC with runtime `cpufeatures` AVX2 backend) and **`chacha20poly1305`** (`v0.11.0`, `431` SLOC): `5f72b84` already pins `chacha20`, so its paired Poly1305 MAC used by `matrix-sdk-store-encryption`, `vodozemac`, and `hpke` was left at `"s"`.
3. **`cbc`** (`v0.2.1`, `508` SLOC) and **`hkdf`** (`v0.13.0`, `258` SLOC): encryption/KDF siblings of `aes`/`ctr`/`hmac` used by `vodozemac`, `matrix-sdk-crypto`, and `matrix-sdk-store-encryption` (largely generic, tiny footprint).
4. **`base64-simd`** (`v0.8.0`, `1,801` SLOC): `ratatui-image` encodes kitty and iTerm2 payloads using `base64_simd::STANDARD` (`ratatui-image-11.1.0/src/protocol/kitty.rs:269`, `iterm2.rs:53`), **not** the `base64` crate.
5. **`crc32fast`** (`v1.5.5`, `2,141` SLOC): PNG/deflate checksum loop used by both `png` and `flate2`.

### Why `ruma`, `serde`, `matrix-sdk*` (`event_cache`), `eyeball`, and `moxcms` belong at `"s"` in `release-small`
- **`tantivy`:** Confirmed **absent** from `Cargo.lock` and `cargo tree` (`src/app/search.rs` searches loaded `RoomState` messages in memory).
- **`serde` / `serde_core` and `ruma-*` (`ruma-events` ~28k SLOC, `ruma-common` ~25k SLOC, `ruma-client-api`):** `serde` is mostly trait definitions; `#[derive(Serialize, Deserialize)]` expands directly into `ruma-*` and `matrix-sdk-*` and monomorphizes at `serde_json` / `rmp-serde` call sites (both already at `opt-level = 3`). Pinning `ruma-*` to `3` would likely give back much of the ~30 MB savings (`64.4 -> 34.4 MB`) to unroll/inline giant event-schema match trees.
- **`matrix-sdk-base` (~35k SLOC), `matrix-sdk-ui` (~40k SLOC, `event_cache` + `timeline`), `matrix-sdk-sqlite` (~11.5k SLOC), `eyeball` (`1.6k` SLOC), `eyeball-im` (`1.6k` SLOC), `imbl` (`18k` SLOC):** `eyeball`/`eyeball-im` are thin generic wrappers over `imbl::Vector` instantiated inside `matrix-sdk-base`/`matrix-sdk-ui`. At `opt-level = "s"`, LLVM still runs full optimization passes (mostly tightening inlining/unrolling thresholds), so SQLite (`libsqlite3-sys`), JSON/MsgPack (`serde_json`/`rmp-serde`), store encryption (`blake3`/`chacha20poly1305`/`poly1305`), and E2EE (`vodozemac`) are the right things to pin at `3`.
- **`moxcms` (`42.8k` SLOC):** Large ICC color-management dependency under `image`; leaving it at `"s"` is the right trade-off for size.
- **Note on `matrix-rat` itself (~52.6k SLOC in `src/`):** In `release-small`, the crate itself (including `src/rich.rs` HTML display parsing/wrapping, `src/text.rs`, `src/images.rs`, and `src/ui/*`) compiles at `opt-level = "s"`, which is sensible for an opt-in small profile but worth keeping in mind for large timelines/resizes.

---

## 3. Panic strategy (`unwind` vs `abort`)

**Agree with keeping `unwind`.**

- `src/main.rs:85-92` relies on RAII `Drop` cleanup (`ProfileLock`, `_log_guard` non-blocking log flush, and `TerminalGuard` inside `app::run`), while `src/terminal.rs:151-189` pairs `TerminalGuard::drop` with a panic hook calling `restore()`.
- Across `src/matrix/` (`rooms.rs`, `manage.rs`, `verify.rs`, `media.rs`, `upload.rs`, `notify.rs`, `keyring.rs`, `mod.rs`) and `src/portal.rs`, background work uses `tokio::spawn` and `tokio::task::JoinSet`. With `panic = "abort"`, a panic in any background task kills the entire client immediately via `SIGABRT` without running `Drop` handlers or isolating task failure.
- Size savings would also be modest: in `target/size/release/matrix-rat`, `.eh_frame_hdr` + `.eh_frame` + `.gcc_except_table` total ~3.98 MB (`0x052e84 + 0x22c938 + 0x14cdc0` bytes), of which `.gcc_except_table` is ~1.36 MB (on glibc Linux, `.eh_frame` largely stays unless `-C force-unwind-tables=no` is also passed).

---

## 4. Should `release-small` be the default for `v0.1.0`?

**No — keep `[profile.release]` (`lto = "fat"`, `cgu = 1`) as default and `[profile.release-small]` opt-in.**

1. `[profile.release]` already cuts stripped size by ~30% (`91.9 MB -> 64.4 MB`) with zero runtime speed penalty (whole-program optimization typically equals or beats ThinLTO).
2. The ~115 / 910 / 620 ms measurements on a small local homeserver are I/O-bound and do not test initial sync, large encrypted SQLite caches (where `blake3`/`poly1305` currently drop to `"s"`), or rapid scrolling/resizing in large rooms (`matrix-rat` UI/text/HTML paths at `"s"`).
3. Shipping a manually curated `-Os` + per-crate `opt-level = 3` allowlist as the default makes future dependency updates fragile (new hot crates silently fall back to `"s"`). Offering `cargo build --profile release-small` for size-sensitive packaging gives the option without risking `v0.1.0` regressions.

---

## 5. `default.nix`

**Leave `default.nix` unchanged.**

- `pkgs.rustPlatform.buildRustPackage` defaults to `cargoBuildType = "release"`, so Nix builds automatically pick up `lto = "fat"` and `codegen-units = 1` (~30% smaller binary) with no Nix changes required.
- `strip = true` works cleanly with `default.nix` (no `separateDebugInfo = true`), and Nix's `cargoUseSystemLibs` hook links `pkgs.aws-lc` and `pkgs.sqlite` directly.
- Keeping `default.nix` on `[profile.release]` matches the recommendation in §4.

---

## Minor note

- In `5f72b84:Cargo.toml` line 60, `(92 MB -> 64 MB)` is specific to the stripped non-Nix `hpserver` build (exact sizes vary by toolchain/platform and Nix shared `aws-lc`/`sqlite`), which is fine as an approximation or phrased `(e.g. ~92 MB -> ~64 MB)`.
