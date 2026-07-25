{
  description = "Rockbox python-web runtime — FastAPI + httpx + SQLAlchemy.";

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
        py = pkgs.python313;
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            py
            py.pkgs.httpx
            py.pkgs.requests
            py.pkgs.fastapi
            py.pkgs.uvicorn
            py.pkgs.pydantic
            py.pkgs.sqlalchemy
            py.pkgs.aiohttp
            py.pkgs.websockets
          ];
          shellHook = ''
            export PYTHONDONTWRITEBYTECODE=1
            export PYTHONUNBUFFERED=1
            export LANG=C.UTF-8
          '';
        };
      }
    );
}
