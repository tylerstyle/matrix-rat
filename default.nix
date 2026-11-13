# Builds matrix-rat with Nix, without flakes:
#
#   nix-build              # result/bin/matrix-rat
#   nix-env -if .          # install into your profile
#
# or in a NixOS / home-manager configuration:
#
#   environment.systemPackages = [ (pkgs.callPackage /path/to/matrix-rat { }) ];
{ pkgs ? import <nixpkgs> { }, lib ? pkgs.lib, rustPlatform ? pkgs.rustPlatform }:
rustPlatform.buildRustPackage {
  pname = "matrix-rat";
  version = (lib.importTOML ./Cargo.toml).package.version;

  # Only what the build reads, so a 10 GB target/ directory is never copied.
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./themes ./contrib ];
  };
  cargoLock.lockFile = ./Cargo.lock;

  # aws-lc-rs (TLS) and the bundled SQLite compile C code.
  nativeBuildInputs = with pkgs; [ pkg-config cmake perl ];
  # nixpkgs tells the -sys crates to link the system libraries instead of building
  # their bundled copies.
  buildInputs = [ pkgs.aws-lc pkgs.sqlite ];

  # The test suite runs in scripts/check.sh; skipping it here halves the build time.
  doCheck = false;

  meta = {
    description = "A mouse-friendly, themeable Matrix client for the terminal";
    homepage = "https://github.com/tylerstyle/matrix-rat";
    license = lib.licenses.asl20;
    mainProgram = "matrix-rat";
    platforms = lib.platforms.linux;
  };
}
