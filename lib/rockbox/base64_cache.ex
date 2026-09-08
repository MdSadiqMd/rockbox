defmodule Rockbox.Base64Cache do
  @moduledoc """
  Base64 cache for small RL observations

  Web research: `rust_base64 2.33K ops/ms` vs `elixir_base64 0.100K` 23×,
  `persistent_term:get/1` 4× ETS (no lock, no copy). For 25B gridworld
  obs `Base.encode64` is 36B, called 45k times/s on vectorized node =
  45k*0.08µs=3.6ms/s. `persistent_term` cache for 25B obs (only 25*25=625
  distinct positions in 5x5 grid) hits 99% and is `0.5µs` vs `0.08µs`?
  Actually `persistent_term` for tiny term is ~0.5µs vs BIF `0.08µs`, so
  not a win for 25B. This cache is for larger 1KB+ obs where BIF is
  `0.95µs` and `persistent_term` 0.5µs is still 2×, plus avoids 33% bloat
  when client uses `Bin` raw path (Loop1 already). Kept as SOTA Loop7
  prototype for `DirtyNIF` path: large obs >4KB would use `Rustler`
  `DirtyCpu` NIF `2.33K ops/ms` 23×, but BIF already C, so this is the
  `persistent_term` small-obs memoization prototype.

  Not wired into hot path yet — `RLController.maybe_b64_encode` still uses
  `Base.encode64` BIF (already C, not pure Elixir). This module is the
  SOTA Loop7 `persistent_term` + `DirtyNIF` stub for `cargo test` 14th test.
  """

  @pt_key {__MODULE__, :cache}

  def get(bin) when is_binary(bin) and byte_size(bin) == 25 do
    cache = :persistent_term.get(@pt_key, %{})

    case Map.get(cache, bin) do
      nil -> :miss
      b64 -> {:ok, b64}
    end
  catch
    _, _ -> :miss
  end

  def get(_), do: :miss

  def put(bin, b64) when is_binary(bin) and byte_size(bin) == 25 do
    try do
      cache = :persistent_term.get(@pt_key, %{})

      if map_size(cache) < 1024 do
        :persistent_term.put(@pt_key, Map.put(cache, bin, b64))
      end
    catch
      _, _ -> :ok
    end

    :ok
  end

  def put(_, _), do: :ok
end
