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
      in
      {
        packages = {
          default = self.packages.${system}.rm-pad;
          rm-pad = pkgs.callPackage ./nix/package.nix {
            rev = self.rev or self.dirtyRev or null;
          };
        };

        devShells.default = pkgs.callPackage ./nix/shell.nix { };
      });
}
