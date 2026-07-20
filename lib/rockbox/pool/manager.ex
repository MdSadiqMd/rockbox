defmodule Rockbox.Pool.Manager do
  @moduledoc """
  Per-(workspace, language) hot/cold pool

  State is held inside the GenServer; pool inventories are kept in a small
  ETS table for O(1) reads from controllers. The manager is the single
  serialiser for spawn/retire decisions per workspace+language

  Acquire path:
  1. Hot pool first (O(1) take). Marks VM busy
  2. Cold/autoscale: synchronous spawn of a new VM via `VM.Supervisor.start_vm`
  3. Quota exceeded → `{:error, :concurrency_exceeded}` propagated up

  Release path:
  - On request completion: VM goes back to hot pool (if mode supports reuse)
    or is destroyed (one-shot exec mode)
  """

  use GenServer

  alias Rockbox.QuotaTracker
  alias Rockbox.Settings.Effective

  @table :rockbox_pool

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  @doc """
  Acquire a VM matching `settings`. Returns `{:ok, vm_id}` or
  `{:error, reason}`.
  """
  def acquire(%Effective{} = s), do: GenServer.call(__MODULE__, {:acquire, s}, 10_000)

  @doc "Release a VM back to the pool (or destroy if one-shot)."
  def release(vm_id, settings), do: GenServer.cast(__MODULE__, {:release, vm_id, settings})

  @doc "Pool snapshot used by the Autoscaler."
  def snapshot, do: :ets.tab2list(@table)

  @doc """
  Retire up to `count` idle VMs from the pool bucket keyed by `key`.

  Called by the Autoscaler on sustained low utilisation. Only VMs currently
  marked `:idle` are touched — busy VMs are left alone. Retirement order is
  FIFO by `ts` (oldest idle first), so long-lived warm VMs age out first and
  freshly-returned VMs stay hot.

  Session-mode VMs release their SessionRouter mapping so subsequent lookups
  for that session_id take the miss path and spawn a fresh VM.
  """
  def retire(key, count) when is_integer(count) and count > 0,
    do: GenServer.cast(__MODULE__, {:retire, key, count})

  def retire(_key, _count), do: :ok

  @impl true
  def init(_) do
    :ets.new(@table, [:named_table, :public, read_concurrency: true])
    {:ok, %{}}
  end

  @impl true
  def handle_call({:acquire, %Effective{} = s}, _from, state) do
    key = pool_key(s)

    case QuotaTracker.reserve(s.workspace_id) do
      :ok ->
        case take_from_pool(key) do
          {:ok, vm_id} ->
            mark_busy(vm_id, key)
            {:reply, {:ok, vm_id}, state}

          :empty ->
            case spawn_vm(s) do
              {:ok, vm_id} ->
                mark_busy(vm_id, key)
                {:reply, {:ok, vm_id}, state}

              {:error, reason} ->
                QuotaTracker.release(s.workspace_id)
                {:reply, {:error, reason}, state}
            end
        end

      err ->
        {:reply, err, state}
    end
  end

  @impl true
  def handle_cast({:release, vm_id, %Effective{} = s}, state) do
    case s.mode do
      :exec ->
        # One-shot: destroy.
        Rockbox.VM.Supervisor.stop_vm(vm_id, :normal)
        :ets.delete(@table, vm_id)
        QuotaTracker.release(s.workspace_id)

      _other ->
        # Session / RL: return to hot pool for reuse.
        :ets.insert(
          @table,
          {vm_id, %{key: pool_key(s), state: :idle, ts: System.system_time(:millisecond)}}
        )
    end

    {:noreply, state}
  end

  def handle_cast({:vm_dead, vm_id, workspace_id}, state) do
    :ets.delete(@table, vm_id)
    QuotaTracker.release(workspace_id)
    {:noreply, state}
  end

  def handle_cast({:retire, key, count}, state) do
    require Logger

    victims =
      @table
      |> :ets.select([
        {{:"$1", %{key: key, state: :idle, ts: :"$2"}}, [], [{{:"$1", :"$2"}}]}
      ])
      |> Enum.sort_by(fn {_vm, ts} -> ts end)
      |> Enum.take(count)

    workspace_id =
      case key do
        {w, _l, _r, _m, _s} -> w
        _ -> nil
      end

    Enum.each(victims, fn {vm_id, _ts} ->
      Logger.debug("autoscaler retire vm_id=#{vm_id} key=#{inspect(key)}")

      case key do
        {_w, _l, _r, :session, sid} when is_binary(sid) ->
          Rockbox.SessionRouter.forget(sid)

        _ ->
          :ok
      end

      Rockbox.VM.Supervisor.stop_vm(vm_id, :autoscale_retire)
      :ets.delete(@table, vm_id)

      if is_binary(workspace_id), do: QuotaTracker.release(workspace_id)
    end)

    {:noreply, state}
  end

  defp pool_key(%Effective{workspace_id: w, language: l, runtime: r, mode: m, session_id: s}),
    do: {w, l, r, m, s}

  defp take_from_pool(key) do
    matches =
      :ets.match_object(@table, {:"$1", %{key: key, state: :idle, ts: :_}}, 1)

    case matches do
      {[{vm_id, _entry}], _cont} -> {:ok, vm_id}
      _ -> :empty
    end
  end

  defp mark_busy(vm_id, key) do
    :ets.insert(@table, {vm_id, %{key: key, state: :busy, ts: System.system_time(:millisecond)}})
  end

  defp spawn_vm(%Effective{} = s) do
    vm_id = "vm_" <> (:crypto.strong_rand_bytes(8) |> Base.encode16(case: :lower))

    case Rockbox.VM.Supervisor.start_vm(%{
           vm_id: vm_id,
           workspace_id: s.workspace_id,
           language: s.language,
           mode: s.mode,
           session_id: s.session_id,
           runtime: s.runtime
         }) do
      {:ok, _pid} -> {:ok, vm_id}
      {:error, reason} -> {:error, reason}
    end
  end
end
