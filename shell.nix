{ pkgs ? import <nixpkgs> { } }:
pkgs.mkShell {
  # Rust comes from the system; this shell adds what the C dependencies need
  # (aws-lc-rs for TLS, bundled SQLite).
  nativeBuildInputs = with pkgs; [ pkg-config cmake perl gcc gnumake ];
  buildInputs = with pkgs; [ ];
}
