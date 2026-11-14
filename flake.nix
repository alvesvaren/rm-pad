{
  description = "rm-pad development environment and packages";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/release-26.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        inherit (pkgs) lib;
        # onTagBinBranch is true when nix/bin-lock.json exists, i.e. on the
        # `tag-bin` branch that pins the prebuilt binary. The source package
        # lives on `main`; the two are mutually exclusive per branch.
        common = pkgs.callPackage ./nix/common.nix { };
      in
      {
        packages = {
          default =
            if common.onTagBinBranch
            then self.packages.${system}.rm-pad-bin
            else self.packages.${system}.rm-pad;

          # Build-from-source package (the `main` branch).
          rm-pad =
            assert lib.assertMsg (!common.onTagBinBranch) ''
              rm-pad: the `tag-bin` branch only carries the prebuilt binary package (rm-pad-bin).
              To build rm-pad from source, use the `main` branch instead, e.g.
                nix run github:alvesvaren/rm-pad#rm-pad'';
            pkgs.callPackage ./nix/package.nix {
              rev = self.rev or self.dirtyRev or null;
            };

          # Prebuilt release binary, pinned in nix/bin-lock.json by
          # .github/workflows/update-bin-lock.yml on the `tag-bin` branch. The
          # lock file is only read once the branch assertion passes.
          rm-pad-bin =
            assert lib.assertMsg common.onTagBinBranch ''
              rm-pad-bin: the prebuilt binary package is only pinned on the `tag-bin` branch.
              Use it from there, e.g.
                nix run github:alvesvaren/rm-pad/tag-bin
              To build rm-pad from source, use the `rm-pad` package on the `main` branch instead.'';
            pkgs.callPackage ./nix/package-bin.nix (
              builtins.fromJSON (builtins.readFile ./nix/bin-lock.json)
            );
        };

        devShells.default = pkgs.callPackage ./nix/shell.nix { };
      });
}
