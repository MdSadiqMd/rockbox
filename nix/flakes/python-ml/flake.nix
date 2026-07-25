{
  description = "Rockbox python-ml runtime — PyTorch + Transformers + Pandas + scikit-learn.";

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
            py.pkgs.numpy
            py.pkgs.scipy
            py.pkgs.pandas
            py.pkgs.scikit-learn
            py.pkgs.matplotlib
            py.pkgs.pillow
            py.pkgs.torch
            py.pkgs.transformers
            py.pkgs.datasets
            py.pkgs.tokenizers
            py.pkgs.accelerate
            py.pkgs.sentencepiece
          ];
          shellHook = ''
            export PYTHONDONTWRITEBYTECODE=1
            export PYTHONUNBUFFERED=1
            export LANG=C.UTF-8
            export MPLBACKEND=Agg
            export OMP_NUM_THREADS=4
            export MKL_NUM_THREADS=4
          '';
        };
      }
    );
}
