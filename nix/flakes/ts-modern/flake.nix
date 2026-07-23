{
  description = "Rockbox ts-modern runtime — Node 22 + tsx + zod + axios + drizzle-orm.";

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
            pkgs.nodejs_22
            pkgs.nodePackages.typescript
            pkgs.nodePackages.tsx
          ];
          shellHook = ''
            export NODE_NO_WARNINGS=1
            export LANG=C.UTF-8
          '';
        };
      }
    );
}
