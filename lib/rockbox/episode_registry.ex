defmodule Rockbox.EpisodeRegistry do
  @moduledoc """
  Routes RL episodes to their VM across calls

  Backed by `:ets` for O(1) reads, so step requests never need the caller to
  carry (or leak) internal `vm_id` handles — the API surface stays
  `episode_id`-only, mirroring [`Rockbox.SessionRouter`] for session mode.

  Writes go through the GenServer so register/forget are serialised per
  episode and a stale registration can never overwrite a live one.

  Durability: routes are write-through to a DETS table so an orchestrator
  restart keeps episode → VM mappings (the VMs themselves die with the node,
  but the durable-episode resurrection path uses the stored workspace/tier
  metadata to rebuild episodes on a fresh pool).

  Resurrection single-flight: `claim_restore/1` hands out exclusive restore
  rights per episode so concurrent steps racing on a dead VM don't spawn
  duplicate replacement VMs.
  """

  use GenServer

  @table :rockbox_episode_route

  defmodule Route do
    @enforce_keys [:episode_id, :vm_id]
    defstruct [:episode_id, :vm_id, :workspace_id, :tier, :created_at, :last_seen]
  end

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  @doc """
  Resolve an episode to its VM. Returns `{:ok, vm_id}` if known, `:miss` if
  not registered (already torn down or never started).
  """
  def lookup(episode_id) do
    case :ets.lookup(@table, episode_id) do
      [{_, route}] -> {:ok, route.vm_id}
      [] -> :miss
    end
  end

  @doc """
  Resolve an episode to its full route (`{vm_id, workspace_id, tier}`). The
  workspace id + tier let teardown and resurrection paths release exactly the
  slot the episode reserved and re-run with the caller's ceilings.
  """
  def lookup_route(episode_id) do
    case :ets.lookup(@table, episode_id) do
      [{_, route}] -> {:ok, route.vm_id, route.workspace_id, route.tier || :pro}
      [] -> :miss
    end
  end

  @doc "Register (or refresh) the episode → vm mapping."
  def register(episode_id, vm_id, workspace_id, tier \\ :pro),
    do: GenServer.call(__MODULE__, {:register, episode_id, vm_id, workspace_id, tier})

  @doc "Forget the mapping (called on episode destroy / engine death)."
  def forget(episode_id), do: GenServer.call(__MODULE__, {:forget, episode_id})

  @doc """
  Claim exclusive restore rights for `eid`. `:ok` means the caller must
  resurrect the episode; `:in_progress` means another process is doing it and
  the caller should wait for the registry to be updated (or time out).
  Claims are auto-released after `ttl_ms` so a crashed claimant can't wedge
  the episode forever.
  """
  def claim_restore(episode_id, ttl_ms \\ 20_000),
    do: GenServer.call(__MODULE__, {:claim_restore, episode_id, ttl_ms})

  def restore_done(episode_id), do: GenServer.call(__MODULE__, {:restore_done, episode_id})

  @impl true
  def init(_) do
    :ets.new(@table, [:named_table, :public, read_concurrency: true, write_concurrency: true])
    path = Application.get_env(:rockbox, :episode_routes_path)

    dets =
      if is_binary(path) do
        File.mkdir_p(Path.dirname(path))

        case :dets.open_file(dets_ref(), file: String.to_charlist(path), auto_save: 1_000) do
          {:ok, ref} ->
            # Rehydrate routes recorded by a previous incarnation.
            :ets.insert(@table, :dets.match_object(ref, :"$1"))
            ref

          {:error, reason} ->
            require Logger

            Logger.warning(
              "episode registry: dets unavailable (#{inspect(reason)}); routes are memory-only"
            )

            nil
        end
      else
        nil
      end

    {:ok, %{dets: dets, restoring: MapSet.new(), claims: %{}}}
  end

  defp dets_ref do
    __MODULE__.Dets
  end

  @impl true
  def handle_call({:register, eid, vm_id, wid, tier}, _from, state) do
    now = System.system_time(:millisecond)

    route = %Route{
      episode_id: eid,
      vm_id: vm_id,
      workspace_id: wid,
      tier: tier,
      created_at: now,
      last_seen: now
    }

    :ets.insert(@table, {eid, route})
    persist(state.dets, eid, route)
    {:reply, :ok, state}
  end

  def handle_call({:forget, eid}, _from, state) do
    :ets.delete(@table, eid)
    unpersist(state.dets, eid)
    {:reply, :ok, state}
  end

  def handle_call({:claim_restore, eid, ttl_ms}, {from, _}, %{restoring: r, claims: c} = state) do
    cond do
      MapSet.member?(r, eid) ->
        {:reply, :in_progress, state}

      true ->
        timer = Process.send_after(self(), {:restore_timeout, eid}, ttl_ms)

        {:reply, :ok,
         %{state | restoring: MapSet.put(r, eid), claims: Map.put(c, eid, {from, timer})}}
    end
  end

  def handle_call({:restore_done, eid}, _from, %{restoring: r, claims: c} = state) do
    case Map.get(c, eid) do
      {_pid, timer} -> Process.cancel_timer(timer)
      nil -> :ok
    end

    {:reply, :ok, %{state | restoring: MapSet.delete(r, eid), claims: Map.delete(c, eid)}}
  end

  @impl true
  def handle_info({:restore_timeout, eid}, %{restoring: r, claims: c} = state) do
    require Logger
    Logger.warning("episode registry: restore claim timed out for #{eid}")
    {:noreply, %{state | restoring: MapSet.delete(r, eid), claims: Map.delete(c, eid)}}
  end

  def handle_info(_other, state), do: {:noreply, state}

  defp persist(nil, _eid, _route), do: :ok
  defp persist(dets, eid, route), do: :dets.insert(dets, {eid, route})

  defp unpersist(nil, _eid), do: :ok
  defp unpersist(dets, eid), do: :dets.delete(dets, eid)
end
