{ pkgs ? import <nixpkgs> { } }:
pkgs.mkShell {
  # The whole Rust toolchain comes from one nixpkgs, so rustc, clippy and rustfmt always
  # match (clippy from a different release than rustc fails with E0514). Plus what the C
  # dependencies need (aws-lc-rs for TLS, bundled SQLite).
  nativeBuildInputs = with pkgs; [ rustc cargo clippy rustfmt pkg-config cmake perl gcc gnumake ];
}
