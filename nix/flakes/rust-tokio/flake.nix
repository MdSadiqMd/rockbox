{
  description = "Rockbox rust-tokio runtime — Rust stable + tokio + serde + axum + sqlx.";

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
            pkgs.rustc
            pkgs.cargo
            pkgs.rust-analyzer
            pkgs.pkg-config
            pkgs.openssl
          ];
          shellHook = ''
            export LANG=C.UTF-8
            export CARGO_HOME=/tmp/cargo
          '';
        };
      }
    );
}
