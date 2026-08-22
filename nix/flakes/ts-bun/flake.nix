{
  description = "Rockbox ts-bun runtime — Bun, native TypeScript execution.";

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
            pkgs.bun
          ];
          shellHook = ''
            export BUN_INSTALL=/tmp/.bun
            export XDG_CACHE_HOME=/tmp/.cache
            export LANG=C.UTF-8
          '';
        };
      }
    );
}
