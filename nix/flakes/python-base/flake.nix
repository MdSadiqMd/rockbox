{
  description = "Rockbox python-base runtime — CPython 3.13 + stdlib only.";

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
          packages = [ pkgs.python313 ];
          shellHook = ''
            export PYTHONDONTWRITEBYTECODE=1
            export PYTHONUNBUFFERED=1
            export LANG=C.UTF-8
          '';
        };
      }
    );
}
