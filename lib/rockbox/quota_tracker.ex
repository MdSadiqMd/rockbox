defmodule Rockbox.QuotaTracker do
  @moduledoc """
  Per-workspace concurrency + token-bucket rate limit

  Two budgets:
  - Concurrency cap: how many VMs a workspace may have in-flight at once
    Enforced before pool acquire to keep cgroup-pool fair across tenants
  - Rate limit (token bucket): how many requests-per-second a workspace
    may submit. Burst = `tokens_max`, refill = `tokens_per_s`

  Backed by `:ets` for lock-free reads; writes go through the GenServer so
  the bucket math is serialised per workspace
  """

  use GenServer

  @table :rockbox_quota

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  @doc """
  Try to reserve a VM slot. Returns `:ok` if under the cap, `{:error,
  :concurrency_exceeded}` otherwise. Caller MUST balance with [`release/1`].
  """
  def reserve(workspace_id), do: GenServer.call(__MODULE__, {:reserve, workspace_id})

  def release(workspace_id), do: GenServer.call(__MODULE__, {:release, workspace_id})

  @doc "Returns `:ok` if the bucket has tokens, `{:error, :rate_limited}` otherwise."
  def take_token(workspace_id), do: GenServer.call(__MODULE__, {:take, workspace_id})

  def in_flight(workspace_id) do
    case :ets.lookup(@table, {:in_flight, workspace_id}) do
      [{_, n}] -> n
      [] -> 0
    end
  end

  @impl true
  def init(_) do
    :ets.new(@table, [:named_table, :public, read_concurrency: true])
    {:ok, %{buckets: %{}}}
  end

  @impl true
  def handle_call({:reserve, wid}, _from, state) do
    cap = workspace_cap(wid)
    current = :ets.update_counter(@table, {:in_flight, wid}, {2, 1}, {{:in_flight, wid}, 0})

    if current > cap do
      :ets.update_counter(@table, {:in_flight, wid}, {2, -1})
      {:reply, {:error, :concurrency_exceeded}, state}
    else
      {:reply, :ok, state}
    end
  end

  def handle_call({:take, wid}, _from, state) do
    now = System.monotonic_time(:millisecond)
    {tokens_max, tokens_per_s} = bucket_config(wid)
    bucket = Map.get(state.buckets, wid, {tokens_max, now})

    new_bucket = refill(bucket, tokens_max, tokens_per_s, now)

    case new_bucket do
      {t, ts} when t >= 1 ->
        {:reply, :ok, %{state | buckets: Map.put(state.buckets, wid, {t - 1, ts})}}

      _ ->
        {:reply, {:error, :rate_limited},
         %{state | buckets: Map.put(state.buckets, wid, new_bucket)}}
    end
  end

  @impl true
  def handle_call({:release, wid}, _from, state) do
    :ets.update_counter(@table, {:in_flight, wid}, {2, -1, 0, 0})
    {:reply, :ok, state}
  end

  defp workspace_cap(_wid) do
    # Future: load from Postgres `workspaces.concurrent_max`.
    Application.get_env(:rockbox, :workspace_concurrency_default, 50)
  end

  defp bucket_config(_wid) do
    {Application.get_env(:rockbox, :rate_burst, 20),
     Application.get_env(:rockbox, :rate_per_s, 10)}
  end

  defp refill({tokens, last_ts}, max, per_s, now) do
    elapsed = max(0, now - last_ts) / 1000.0
    refilled = min(max, tokens + elapsed * per_s)
    {refilled, now}
  end
end
