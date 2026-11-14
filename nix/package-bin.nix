{
  lib,
  stdenv,
  callPackage,
  fetchurl,
  autoPatchelfHook,
  # Pinned release artefact, kept up to date by .github/workflows/update-bin-lock.yml
  # (see bin-lock.json). Purely evaluatable, so this works via
  # `nix run github:alvesvaren/rm-pad/tag-bin`.
  tag,
  url,
  sha256,
}:
let
  # The released binary links against the same libraries rm-pad builds against
  # (openssl for ssh2, zlib for libz-sys, libxcb/wayland/libxkbcommon for
  # display-info), so reuse the shared list.
  inherit (callPackage ./common.nix { }) runtimeLibs;
in
assert lib.assertMsg (stdenv.hostPlatform.system == "x86_64-linux") ''
  rm-pad-bin: prebuilt binaries are only published for x86_64-linux (got ${stdenv.hostPlatform.system}).
  Build rm-pad from source (the `rm-pad` package on the `nix` branch) instead.'';
stdenv.mkDerivation {
  pname = "rm-pad-bin";
  version = lib.removePrefix "v" tag;

  src = fetchurl { inherit url sha256; };

  nativeBuildInputs = [ autoPatchelfHook ];
  buildInputs = runtimeLibs ++ [ stdenv.cc.cc.lib ];
  # display-info dlopens some of these; force them onto the rpath.
  runtimeDependencies = runtimeLibs;

  # The tarball is a flat archive (./rm-pad, ./data, ./README.md, ...).
  sourceRoot = ".";

  installPhase = ''
    runHook preInstall
    install -Dm755 rm-pad "$out/bin/rm-pad"
    install -Dm644 data/50-uinput.rules "$out/lib/udev/rules.d/50-uinput.rules"
    install -Dm644 data/rm-pad.service "$out/lib/systemd/user/rm-pad.service"
    install -Dm644 rm-pad.toml.example "$out/share/rm-pad/rm-pad.toml.example"
    install -Dm644 README.md "$out/share/doc/rm-pad/README.md"
    runHook postInstall
  '';

  meta = {
    description = "Forward reMarkable tablet input to your computer (pinned prebuilt release binary)";
    homepage = "https://github.com/alvesvaren/rm-pad";
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "rm-pad";
    platforms = [ "x86_64-linux" ];
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
