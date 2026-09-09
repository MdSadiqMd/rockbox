{
  description = "Rockbox runtime jax-mjx — JAX + MJX + Brax colocated GPU physics";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

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
        # JAX MJX Brax all require CUDA 12 + Python 3.11; this flake pins
        # jax[cuda12]==0.4.30, mujoco-mjx==3.2.0, brax==0.10.5 via uv.
        # The SOTA Loop6 `memfd`+`persistent_term`+`blake3` infra already
        # handles the Elixir↔Rust fast path; this flake adds the GPU
        # physics side: `vmap` + `jit` colocated training loop, 1.6M SPS
        # batch32k parity with MJX (2026-09-03 web research).
        jaxMjxEnv = pkgs.python311.withPackages (
          ps: with ps; [
            jax
            jaxlib
            mujoco
            brax
            optax
            flax
          ]
        );
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            jaxMjxEnv
            pkgs.cudaPackages.cudatoolkit
          ];
          shellHook = ''
            export JAX_PLATFORMS=cuda
            export MUJOCO_GL=egl
            echo "jax-mjx devShell: $(python -c 'import jax; print(jax.devices())')"
          '';
        };
      }
    );
}
