{
  lib,
  callPackage,
  rustPlatform,
  pkg-config,
  # Full commit hash, threaded in from the flake (self.rev). Falls back to the
  # Cargo.toml version for non-flake `callPackage` builds where there is no rev.
  rev ? null,
}:
let
  common = callPackage ./common.nix { };
in
rustPlatform.buildRustPackage ({
  pname = "rm-pad";
  version =
    if rev != null then rev
    else (lib.importTOML ../Cargo.toml).package.version;

  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: _type:
      let base = baseNameOf path;
      in !(builtins.elem base [ "target" ".direnv" "result" ]);
  };

  cargoLock.lockFile = ../Cargo.lock;

  # build.rs cross-compiles the ARM helper; openssl/display-info need pkg-config.
  nativeBuildInputs = [ pkg-config ] ++ common.crossCompilers;
  buildInputs = common.runtimeLibs;

  # Ship the same data files as the Arch package and the -bin package: the udev
  # rule, systemd user unit (relocated to share/systemd/user by a stdenv hook),
  # and example config. Lets a NixOS module consume `systemd.packages = [ … ]`.
  postInstall = ''
    install -Dm644 data/50-uinput.rules "$out/lib/udev/rules.d/50-uinput.rules"
    install -Dm644 data/rm-pad.service "$out/lib/systemd/user/rm-pad.service"
    install -Dm644 rm-pad.toml.example "$out/share/rm-pad/rm-pad.toml.example"
    install -Dm644 README.md "$out/share/doc/rm-pad/README.md"
  '';

  meta = {
    description = "Forward reMarkable tablet input to your computer as libinput devices";
    homepage = "https://github.com/alvesvaren/rm-pad";
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "rm-pad";
    platforms = lib.platforms.linux;
  };
} // common.env)
