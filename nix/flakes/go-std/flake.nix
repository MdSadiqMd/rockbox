{
  description = "Rockbox go-std runtime — Go 1.23 + stdlib + golang.org/x/*.";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.flake-utils.url = "github:numtide/flake-utils";

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.go_1_23
            pkgs.gopls
          ];
          shellHook = ''
            export LANG=C.UTF-8
            export GOFLAGS="-mod=mod"
            export GOCACHE=/tmp/go-cache
          '';
        };
      }
    );
}
