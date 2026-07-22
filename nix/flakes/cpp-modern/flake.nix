{
  description = "Rockbox cpp-modern runtime — clang 19 + libstdc++14 + abseil + boost.";

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
            pkgs.clang_19
            pkgs.cmake
            pkgs.ninja
            pkgs.abseil-cpp
            pkgs.boost
            pkgs.eigen
            pkgs.catch2_3
            pkgs.nlohmann_json
            pkgs.fmt
            pkgs.spdlog
          ];
          shellHook = ''
            export LANG=C.UTF-8
          '';
        };
      }
    );
}
