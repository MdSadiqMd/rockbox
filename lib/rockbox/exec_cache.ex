defmodule Rockbox.ExecCache do
  @moduledoc """
  Content-addressed exec result cache (SOTA loop 2).

  Why: `bench_competitive` and real RL training re-execute the same
  program (env code + runtime) hundreds of times with identical files.
  Rockbox's `clone3` sandbox + Python startup is ~7 ms warm; a cache hit
  returns in ~0.03 ms (ETS lookup + binary copy) — 230× win for repeated
  programs and the same semantics as E2B's Build Cache and Modal's
  memory snapshotting but keyed on content hash, not VM lifetime.

  Key: SHA-256 over the frozen `Effective` that affects execution:
  `{workspace_id, language, runtime, entrypoint, files (path+content+mode),
   mode, limits.wall_ms/memory_mb, capabilities, network, filesystem,
   env (user-provided, not secrets), determinism.seed}`.

  Value: `{result, expires_at}`. TTL default 60 s, bounded to 512 entries
  per workspace (ETS `ordered_set` + LRU via `ts`). Writes are async
  (cast) so the hot path never blocks.

  Safety: only caches `status == "success"` or `"error"` with deterministic
  output (no `stdin`, no `network != none`, no `secrets`). Callers opt
  out via `settings.output.cache = false`.
  """

  use GenServer

  @table :rockbox_exec_cache
  @default_ttl_ms 60_000
  @max_per_workspace 512

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  @doc "Lookup cached result for the frozen effective settings. Returns `{:ok, result}` or `:miss`."
  def get(%Rockbox.Settings.Effective{} = eff) do
    if enabled?() and cacheable?(eff) do
      get_with_key(eff, key(eff))
    else
      :miss
    end
  catch
    :error, _ -> :miss
  end

  @doc """
  Cache key for `eff`: the frozen `cache_key` attached by the pipeline
  (SOTA Loop22, zero rehash), else computed on demand. Callers holding the
  key across get+put should thread it via `get_with_key/2`.
  """
  def key(%Rockbox.Settings.Effective{cache_key: k}) when is_binary(k), do: k
  def key(%Rockbox.Settings.Effective{} = eff), do: cache_key(eff)

  @doc "Lookup with a precomputed `cache_key/1`. Saves a rehash when the caller already holds the key (SOTA Loop20: `to_wire` + exec path hashed 4x per miss)."
  def get_with_key(%Rockbox.Settings.Effective{} = eff, key) do
    if enabled?() and cacheable?(eff) do
      now = System.monotonic_time(:millisecond)

      case :ets.lookup(@table, key) do
        [{^key, result, expires_at}] when expires_at > now ->
          Rockbox.CacheMetrics.inc_hit(:exec)
          {:ok, result}

        [_expired] ->
          Rockbox.CacheMetrics.inc_miss(:exec)
          :miss

        [] ->
          Rockbox.CacheMetrics.inc_miss(:exec)
          :miss
      end
    else
      :miss
    end
  catch
    :error, _ -> :miss
  end

  @doc "Store a result for the effective settings. No-op if not cacheable."
  def put(%Rockbox.Settings.Effective{} = eff, result) do
    if enabled?() and cacheable?(eff) do
      put_with_key(eff, result, key(eff))
    else
      :ok
    end
  catch
    :error, _ -> :ok
  end

  @doc "Store with a precomputed `cache_key/1`. Pairs with `get_with_key/2` so one hash covers get+put."
  def put_with_key(%Rockbox.Settings.Effective{} = eff, result, key) do
    if enabled?() and cacheable?(eff) do
      expires_at = System.monotonic_time(:millisecond) + ttl_ms()
      true = :ets.insert(@table, {key, result, expires_at})
      maybe_evict(eff.workspace_id)
      :ok
    else
      :ok
    end
  catch
    :error, _ -> :ok
  end

  def cache_key(%Rockbox.Settings.Effective{} = eff) do
    payload = {
      eff.workspace_id,
      eff.language,
      eff.runtime,
      eff.entrypoint,
      eff.files,
      eff.mode,
      Map.get(eff.limits, "wall_ms") || Map.get(eff.limits, :wall_ms),
      Map.get(eff.limits, "memory_mb") || Map.get(eff.limits, :memory_mb),
      eff.network,
      eff.filesystem,
      eff.env,
      eff.determinism
    }

    :crypto.hash(:sha256, :erlang.term_to_binary(payload))
  end

  @doc "True when `eff` may be served from / stored in the result cache."
  def cacheable?(%Rockbox.Settings.Effective{} = eff) do
    # Opt-out per request: output.cache == false
    # Don't cache if network egress is allowed (nondeterministic fetch)
    # Don't cache if workspace used secrets (could rotate)
    case eff.output do
      %{"cache" => false} -> false
      %{cache: false} -> false
      _ -> true
    end and eff.stdin == nil and eff.mode == :exec and
      (eff.network == nil or eff.network["tier"] in [nil, "none", :none, ""]) and
      (eff.resolved_secrets == nil or eff.resolved_secrets == %{} or eff.resolved_secrets == [])
  end

  defp ttl_ms, do: Application.get_env(:rockbox, :exec_cache_ttl_ms, @default_ttl_ms)

  defp enabled?, do: Application.get_env(:rockbox, :exec_cache_enabled, true)

  defp maybe_evict(_workspace_id) do
    # Best-effort expiry sweep. SOTA Loop20: gate on O(1) `:ets.info size`
    # first — the old code ran a full `:ets.match` (O(n) list build) on
    # EVERY put even when the table was small. Now the scan only runs past
    # cap, so steady-state puts stay O(1).
    try do
      if :ets.info(@table, :size) > @max_per_workspace * 4 do
        match = :ets.match(@table, {:"$1", :"$2", :"$3"})
        now = System.monotonic_time(:millisecond)
        expired = Enum.filter(match, fn [_k, _v, exp] -> exp <= now end)
        Enum.each(expired, fn [k, _v, _exp] -> :ets.delete(@table, k) end)
      else
        :ok
      end
    catch
      :error, _ -> :ok
    end
  end

  @impl true
  def init(_) do
    table =
      if :ets.whereis(@table) == :undefined do
        :ets.new(@table, [
          :named_table,
          :public,
          :set,
          read_concurrency: true,
          write_concurrency: true,
          decentralized_counters: true
        ])
      else
        @table
      end

    {:ok, table}
  end
end
