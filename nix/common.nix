{
  writeShellScriptBin,
  openssl,
  libxcb,
  wayland,
  libxkbcommon,
  zlib,
  pkgsCross,
}:
let
  # Cross toolchain for the evdev grab helper (build.rs compiles
  # helper/evgrab.c for the reMarkable's ARM CPUs). The helper links with
  # -static, so wrap each cross gcc to also search the static glibc, whose
  # libc.a lives in a separate output. Using the glibc (not musl) toolchains
  # keeps everything in the binary cache instead of building gcc from source.
  mkCc = crossPkgs:
    let
      cc = crossPkgs.stdenv.cc;
      staticLibc = crossPkgs.glibc.static;
      name = "${cc.targetPrefix}gcc";
    in
    writeShellScriptBin name ''
      exec ${cc}/bin/${name} -L${staticLibc}/lib "$@"
    '';

  armv7Cc = mkCc pkgsCross.armv7l-hf-multiplatform;
  aarch64Cc = mkCc pkgsCross.aarch64-multiplatform;
in
{
  inherit armv7Cc aarch64Cc;

  # Passed to nativeBuildInputs wherever build.rs runs.
  crossCompilers = [ armv7Cc aarch64Cc ];

  # Libraries rm-pad links against, needed both to build and at runtime:
  # openssl (ssh2), zlib (libz-sys), libxcb/wayland/libxkbcommon (display-info).
  runtimeLibs = [
    openssl
    libxcb
    wayland
    libxkbcommon
    zlib
  ];

  # build.rs looks up the ARM cross-compilers by name; point it at the wrappers.
  env = {
    ARMV7_CC = "${armv7Cc}/bin/armv7l-unknown-linux-gnueabihf-gcc";
    AARCH64_CC = "${aarch64Cc}/bin/aarch64-unknown-linux-gnu-gcc";
  };
}
