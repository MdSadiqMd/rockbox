defmodule Rockbox.WireCache do
  @moduledoc """
  Wire payload cache for `Effective.to_wire` (SOTA Loop5, reworked Loop20).

  Why: `Effective.to_wire` builds the msgpack payload for `Port.command`
  on every exec: `Enum.map(files, &file_to_wire/1)` + `Enum.map(caps,
  &Atom.to_string/1)` + `Map.new` etc — ~10µs for 1-file hello.py, ~50µs
  for 5-file ML project. For repeated programs the wire map is identical
  except `request_id`.

  SOTA Loop20 rework: entries are per-key `persistent_term`s
  (`{{__MODULE__, key}, wire}`), NOT one giant `%{hash => wire}` map.
  The old design copied the whole ~512KB cache term and triggered a
  global GC pause on EVERY miss (new distinct program). Per-key puts only
  copy one ~2KB value; the key list (`{__MODULE__, :keys}`, 256 x 32B)
  is the only shared term. Eviction erases the evicted keys' terms and
  rewrites the small list — no full-cache copy, no stop-the-world over
  values.

  Key: `ExecCache.cache_key(eff)` SHA256 of content (2.5µs). Value:
  `wire_map_without_request_id`. Hit clones + re-inserts current
  `request_id` (~0.5µs).
  """

  @max_entries 256
  @keys_key {__MODULE__, :keys}

  def get(%Rockbox.Settings.Effective{} = eff) do
    get_with_key(eff, Rockbox.ExecCache.key(eff))
  end

  @doc "Lookup with a precomputed `ExecCache.cache_key/1`. Pairs with `Effective.to_wire` so one hash covers get+put."
  def get_with_key(%Rockbox.Settings.Effective{} = eff, key) do
    case :persistent_term.get({__MODULE__, key}, nil) do
      nil ->
        Rockbox.CacheMetrics.inc_miss(:wire)
        :miss

      wire_without_id ->
        Rockbox.CacheMetrics.inc_hit(:wire)
        {:ok, Map.put(wire_without_id, "request_id", eff.request_id)}
    end
  catch
    _, _ -> :miss
  end

  def put(%Rockbox.Settings.Effective{} = eff, wire_map) do
    put_with_key(eff, wire_map, Rockbox.ExecCache.key(eff))
  end

  @doc "Store with a precomputed key. Only the entry + small key list are rewritten (no whole-cache copy)."
  def put_with_key(%Rockbox.Settings.Effective{} = eff, wire_map, key) do
    _ = eff
    wire_without_id = Map.delete(wire_map, "request_id")

    try do
      :persistent_term.put({__MODULE__, key}, wire_without_id)
      track_key(key)
    catch
      _, _ -> :ok
    end

    :ok
  end

  def clear do
    try do
      keys = :persistent_term.get(@keys_key, [])
      Enum.each(keys, &:persistent_term.erase({__MODULE__, &1}))
      :persistent_term.erase(@keys_key)
    catch
      _, _ -> :ok
    end

    :ok
  end

  # Newest-first key list, capped. The list term is ~8KB (256 x 32B keys);
  # rewriting it per miss is 64x cheaper than rewriting the old whole-cache
  # map term (~512KB values). Evicted entries' terms are erased so their
  # memory is reclaimed at the next global GC without stalling on values.
  defp track_key(key) do
    keys = :persistent_term.get(@keys_key, [])

    if Enum.member?(keys, key) do
      :ok
    else
      {keep, drop} = Enum.split([key | keys], @max_entries)
      Enum.each(drop, &:persistent_term.erase({__MODULE__, &1}))
      :persistent_term.put(@keys_key, keep)
      :ok
    end
  end
end
