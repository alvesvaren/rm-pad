{
  mkShell,
  lib,
  callPackage,
  rustc,
  cargo,
  rust-analyzer,
  clippy,
  rustfmt,
  pkg-config,
  rustPlatform,
}:
let
  common = callPackage ./common.nix { };
in
mkShell ({
  nativeBuildInputs = [
    rustc
    cargo
    rust-analyzer
    clippy
    rustfmt
    pkg-config
  ] ++ common.crossCompilers;

  buildInputs = common.runtimeLibs;

  # display-info dlopens some of these at runtime.
  LD_LIBRARY_PATH = lib.makeLibraryPath common.runtimeLibs;

  RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
} // common.env)
