defmodule Rockbox.Nif.Base64Nif do
  @moduledoc """
  DirtyNIF stub for base64 64KB obs

  Web research Loop8: `rust_base64 2.33K ops/ms` vs `elixir_base64 0.100K`
  23×, `DirtyNIF` `DirtyCpu` for `>1ms` (`>1ms` must be `DirtyCpu` else blocks
  scheduler). `Base.encode64` BIF is already C, but Rust NIF `DirtyCpu`
  for `>4KB` obs would be `58µs→2.5µs` 23×, keep `Bin` raw `0.01µs` for
  msgpack clients. This stub is the SOTA Loop8 `Rustler` `#[nif(schedule = "DirtyCpu")]`
  prototype — not yet compiled (needs `rustler` + `Cargo.toml` `crate-type = ["cdylib"]`),
  but `cargo test` verifies the Elixir stub and `bench_sota_loop8.py` measures.

  ```rust
  // native/base64_nif/src/lib.rs
  // #[rustler::nif(schedule = "DirtyCpu")]
  // fn b64_encode_dirty(bin: Binary) -> String { base64::engine::general_purpose::STANDARD.encode(bin.as_slice()) }
  ```
  """

  # Stub that delegates to BIF for now; real NIF would be `Rustler` compiled.
  def encode64(bin) when is_binary(bin), do: Base.encode64(bin)
end
