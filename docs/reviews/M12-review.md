# Review: M12 — `release-profile` (`Cargo.toml` `[profile.release]`)

**Commits reviewed:** `5f72b84` and `c2da376` on `origin/release-profile` (`git diff main..c2da376 -- Cargo.toml`: `lto = "fat"`, `codegen-units = 1`, keeps `strip = true`; `release-small` dropped in `c2da376`, so questions 2 and 4 are skipped).  
**Files inspected:** `Cargo.toml` (`main` and `c2da376`), `Cargo.lock`, `default.nix`, `shell.nix`, `src/main.rs`, `src/app.rs` (`TerminalGuard::enter`), `src/terminal.rs`, `src/matrix/` (`spawn`/`JoinSet` call sites), `~/.cargo/registry/src/.../{aws-lc-sys-0.47.0,blake3-1.8.7,libsqlite3-sys-0.38.2}`, `<nixpkgs>` `cargo-setup-hook.sh` / `aws-lc/package.nix`, and `target/size/release/matrix-rat`.

---

## Verdict

**Approve** `[profile.release]` (`lto = "fat"`, `codegen-units = 1`, `strip = true`) for `v0.1.0`. No blocking technical or Nix issues; one minor git hygiene note below (`c2da376` swept the earlier `M12-review.md` draft onto `release-profile`).

---

## 1. Risks of `lto = "fat"` + `codegen-units = 1` (+ `strip = true`)

- **`aws-lc-sys` / `ring` / `libsqlite3-sys` / `blake3` (C & assembly):**
  - `ring` (`0.17.14` in `Cargo.lock`) is **not** built on Linux (`cargo tree --edges no-dev -i ring` is empty; Linux `rustls` / `rustls-webpki` uses `aws-lc-rs v1.19.0` -> `aws-lc-sys v0.47.0`).
  - `aws-lc-sys` (`v0.47.0`), `libsqlite3-sys` (`v0.38.2`), and `blake3` (`v1.8.7`) compile C sources and hand-written `.S`/NASM assembly via `cmake`/`cc` into native ELF object files inside static archives, not LLVM bitcode (`aws-lc-sys`'s `cmake_builder.rs:22-35` also strips `-flto*` from `CFLAGS`). Rust `lto = "fat"` and `codegen-units = 1` apply only across Rust LLVM bitcode crates, and `ld` links those `.a` archives normally (`readelf -d` on `target/size/release/matrix-rat` confirms only `libgcc_s.so.1`, `libm.so.6`, `libc.so.6` are `NEEDED`).
- **Proc-macros and `scripts/check.sh`:**
  - Cargo does not apply `lto` to host proc-macro dylibs (`66` crates in `Cargo.lock` depend on `proc-macro2` / `-derive` / `-macros`), and `[profile.test]` inherits `dev` (not `release`), so `scripts/check.sh` (`cargo test`, `cargo clippy`, `cargo build`) is unaffected.
- **Memory use on a 16 GB CI runner:**
  - Peak RSS comes from LLVM inside `rustc` merging LLVM bitcode across all ~428 active Rust non-dev crates into a single LLVM module before emitting one `.o` for `ld` (rather than `ld` itself).
  - Because crate builds finish before final binary LTO begins, and `Cargo.toml` defines only **one** `[[bin]]` target and no separate `tests/`, `benches/`, or `examples/` targets, `cargo build --release` / `nix-build` runs a single fat-LTO `rustc` process at a time — comfortable within 16 GB. (Avoid running `cargo test --release` or `--all-targets --release` concurrently in smaller memory containers.)
- **Reproducibility with Nix (`default.nix`):**
  - `codegen-units = 1` + `lto = "fat"` eliminates parallel CGU-partitioning variance across threads during Rust codegen, which **helps** determinism in Nix; `strip = true` matches Nix's standard strip phase (`default.nix` does not set `separateDebugInfo`).
  - Under `default.nix`, `nixpkgs`'s `cargo-setup-hook.sh:94-118` exports `AWS_LC_SYS_USE_SYSTEM=1` and `LIBSQLITE3_SYS_USE_PKG_CONFIG=1`, linking `pkgs.aws-lc` and `pkgs.sqlite` (`pkg-config`) as Nix store shared libraries outside Rust LTO anyway (`pkgs/by-name/aw/aws-lc/package.nix:16`).

---

## 3. Panic strategy (`unwind` vs `abort`)

**Agree with keeping `unwind`.**

- `src/main.rs:85-92` relies on RAII `Drop` cleanup (`ProfileLock`, `_log_guard` non-blocking log flush, and `TerminalGuard::enter` at `src/app.rs:2920`), while `src/terminal.rs:151-189` pairs `TerminalGuard::drop` with a panic hook calling `restore()`.
- Across `src/matrix/` (`rooms.rs`, `manage.rs`, `verify.rs`, `media.rs`, `upload.rs`, `notify.rs`, `keyring.rs`, `mod.rs`) and `src/portal.rs`, background work uses `tokio::spawn` and `tokio::task::JoinSet`. With `panic = "abort"`, a panic in any background task terminates the whole client immediately via `SIGABRT` without running `Drop` handlers or isolating task failure.
- Size savings would also be modest: in `target/size/release/matrix-rat`, `.eh_frame_hdr` + `.eh_frame` + `.gcc_except_table` total ~3.98 MB (`0x052e84 + 0x22c938 + 0x14cdc0` bytes), of which `.gcc_except_table` is ~1.36 MB (on glibc Linux, `.eh_frame` largely remains unless `-C force-unwind-tables=no` is also passed).

---

## 5. `default.nix`

**Leave `default.nix` unchanged.**

- `pkgs.rustPlatform.buildRustPackage` defaults to `cargoBuildType = "release"`, so Nix builds automatically get `lto = "fat"` and `codegen-units = 1` (~30% smaller binary) with no Nix changes required.
- `strip = true` works cleanly with `default.nix` (no `separateDebugInfo = true`), and Nix's `cargoUseSystemLibs` links `pkgs.aws-lc` and `pkgs.sqlite` directly.

---

## Minor notes

- **`docs/reviews/M12-review.md` in `c2da376`:** Commit `c2da376` on `origin/release-profile` also included the earlier `release-small` draft of `docs/reviews/M12-review.md`; drop or update it on the branch before merging to avoid carrying stale text or a merge collision with the copy written in `main`.
- **Comment wording (`Cargo.toml:61`):** `(92 MB -> 64 MB)` is specific to the stripped non-Nix `hpserver` binary; fine as-is, or `(e.g. ~92 MB -> ~64 MB)` if preferred.
