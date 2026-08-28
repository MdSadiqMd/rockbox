defmodule Rockbox.ApiKeys.Cache do
  @moduledoc """
  Named ETS owner for the API-key verification cache. Reads are lock-free
  (`read_concurrency`, no GenServer hop); this process exists purely so the
  table has sane supervisor lifecycle/restart semantics.
  """

  use GenServer

  @table :rockbox_api_key_cache

  def start_link(_), do: GenServer.start_link(__MODULE__, nil, name: __MODULE__)

  def get(hash) do
    now = System.system_time(:millisecond)

    case :ets.lookup(@table, hash) do
      [{^hash, ctx, expires_at}] when expires_at > now -> {:ok, ctx}
      [_stale] -> :miss
      [] -> :miss
    end
  catch
    # Table not yet created (boot race / crashed owner): fall through to DB
    :error, _ -> :miss
  end

  def put(hash, ctx) do
    expires_at = System.system_time(:millisecond) + ttl()
    true = :ets.insert(@table, {hash, ctx, expires_at})
    :ok
  catch
    :error, _ -> :ok
  end

  def purge(key_id) do
    :ets.match_delete(@table, {:_, %{key_id: key_id}, :_})
    :ok
  catch
    :error, _ -> :ok
  end

  defp ttl, do: Application.get_env(:rockbox, :api_key_cache_ttl_ms, 60_000)

  @impl true
  def init(nil) do
    table =
      if :ets.whereis(@table) == :undefined do
        :ets.new(@table, [:set, :named_table, :public, read_concurrency: true])
      else
        @table
      end

    {:ok, table}
  end
end
