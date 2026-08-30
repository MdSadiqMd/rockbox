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

  Hot path is lock-free: quota reservation is a pure ETS atomic
  (`update_counter`) and the idle-VM take uses an ETS claim row
  (`insert_new`), so a warm acquire never touches the manager GenServer.
  Only the cold-spawn path (no idle VM) serialises through the GenServer.
  """
  def acquire(%Effective{} = s) do
    key = pool_key(s)

    case QuotaTracker.reserve(s.workspace_id) do
      :ok ->
        case take_idle(key, idle_ttl_ms(s)) do
          {:ok, vm_id, _ts} ->
            mark_busy(vm_id, key)
            {:ok, vm_id}

          :empty ->
            # No idle VM: fall back to the serialised cold-spawn path. The
            # caller's reservation is kept — {:acquire_cold, ...} never
            # reserves again, so usage metering counts one request per
            # acquire and the quota slot can't double-reserve.
            GenServer.call(__MODULE__, {:acquire_cold, s}, 10_000)
        end

      err ->
        err
    end
  end

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

  @doc """
  Remove a VM from the pool after the engine died (crash, kill, OOM, or
  explicit teardown). Idempotent: unknown vm_ids are a no-op.

  Releases the quota slot only if this VM still holds one — VMs that went
  idle released their slot when they entered the pool, so retiring them must
  not decrement the workspace counter a second time.
  """
  def vm_dead(vm_id, workspace_id) when is_binary(vm_id),
    do: GenServer.cast(__MODULE__, {:vm_dead, vm_id, workspace_id})

  @impl true
  def init(_) do
    :ets.new(@table, [:named_table, :public, read_concurrency: true])
    {:ok, %{}}
  end

  @impl true
  # Cold-spawn path entered from acquire/1 with the reservation already held.
  # Also reachable directly (VMController) — that path must reserve first.
  def handle_call({:acquire_cold, %Effective{} = s}, _from, state) do
    key = pool_key(s)
    ttl_ms = idle_ttl_ms(s)

    case take_idle(key, ttl_ms) do
      {:ok, vm_id, _ts} ->
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
  end

  @impl true
  def handle_cast({:release, vm_id, %Effective{} = s}, state) do
    case {s.mode, idle_ttl_ms(s)} do
      {:exec, 0} ->
        # One-shot (TTL disabled): destroy.
        destroy_vm(vm_id, s.workspace_id)

      {_mode, _ttl} ->
        # Reusable: return to hot pool for the next request. The quota slot
        # is freed — the VM exists but is idle, so it must not count towards
        # the concurrency cap for future spawns.
        QuotaTracker.release(s.workspace_id)

        :ets.insert(
          @table,
          {vm_id, %{key: pool_key(s), state: :idle, ts: System.system_time(:millisecond)}}
        )
    end

    {:noreply, state}
  end

  def handle_cast({:vm_dead, vm_id, workspace_id}, state) do
    # A VM holds a quota slot exactly while it is `:busy`. Idle pooled VMs
    # already released their slot, so only release when the dead VM was busy.
    case :ets.lookup(@table, vm_id) do
      [{^vm_id, %{state: :busy}}] -> QuotaTracker.release(workspace_id)
      _ -> :ok
    end

    :ets.delete(@table, vm_id)
    {:noreply, state}
  end

  def handle_cast({:retire, key, count}, state) do
    require Logger

    victims =
      @table
      |> :ets.match_object({:"$1", %{key: key, state: :idle, ts: :"$2"}})
      |> Enum.sort_by(fn {_vm, entry} -> entry.ts end)
      |> Enum.take(count)

    Enum.each(victims, fn {vm_id, _ts} ->
      Logger.debug("autoscaler retire vm_id=#{vm_id} key=#{inspect(key)}")

      case key do
        {_w, _l, _r, :session, sid} when is_binary(sid) ->
          Rockbox.SessionRouter.forget(sid)

        _ ->
          :ok
      end

      :ets.delete(@table, vm_id)
      Rockbox.VM.Supervisor.stop_vm(vm_id, :autoscale_retire)
    end)

    {:noreply, state}
  end

  defp pool_key(%Effective{workspace_id: w, language: l, runtime: r, mode: m, session_id: s}),
    do: {w, l, r, m, s}

  defp idle_ttl_ms(%Effective{lifecycle: lc}) do
    case lc do
      %{"idle_ttl_s" => s} when is_integer(s) -> s * 1000
      _ -> 0
    end
  end

  @doc false
  # Atomically claim one idle VM from the bucket `key`.
  #
  # Claim specs built on `select_replace`/`select_delete`/`select` reject
  # map-pattern heads in this OTP (tuple-construction bodies with map heads
  # are mis-compiled), so the claim uses a sentinel row instead: `insert_new`
  # succeeds for exactly one process, serialising the scan+take for the
  # bucket; concurrent acquirers see the sentinel, return `:empty`, and take
  # the cold-spawn path. Returns `{:ok, vm_id, ts}` — the row is REMOVED
  # from the table, so the caller must either `mark_busy/2` or re-insert the
  # idle row. Returns `:empty` when the bucket is empty or the only
  # candidate is stale (in which case it is destroyed, outside the acquire
  # call, so terminating the engine never blocks other workspaces).
  def take_idle(key, ttl_ms) do
    claim = {:__claim__, %{key: key, state: :claimed, ts: System.system_time(:millisecond)}}

    case :ets.insert_new(@table, claim) do
      true ->
        try do
          take_idle_locked(key, ttl_ms)
        after
          :ets.delete(@table, :__claim__)
        end

      false ->
        :empty
    end
  end

  defp take_idle_locked(key, ttl_ms) do
    case :ets.match_object(@table, {:"$1", %{key: key, state: :idle, ts: :"$2"}}, 1) do
      {[{vm_id, entry}], _} ->
        :ets.delete(@table, vm_id)

        if ttl_ms > 0 and stale?(entry.ts, ttl_ms) do
          # Idle past its TTL: destroy instead of reusing, so exec-mode
          # warm VMs cannot leak indefinitely.
          spawn(fn -> Rockbox.VM.Supervisor.stop_vm(vm_id, :idle_ttl_expired) end)
          :empty
        else
          {:ok, vm_id, entry.ts}
        end

      _ ->
        :empty
    end
  end

  defp stale?(ts, ttl_ms), do: System.system_time(:millisecond) - ts > ttl_ms

  defp destroy_vm(vm_id, workspace_id) do
    case :ets.lookup(@table, vm_id) do
      [{^vm_id, %{state: :busy}}] -> QuotaTracker.release(workspace_id)
      _ -> :ok
    end

    :ets.delete(@table, vm_id)
    Rockbox.VM.Supervisor.stop_vm(vm_id, :normal)
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
