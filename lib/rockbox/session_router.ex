defmodule Rockbox.SessionRouter do
  @moduledoc """
  Routes session-mode requests to the same VM across calls

  Backed by `:ets` for O(1) reads. Writes go through the GenServer so
  reservations (`new` or `attach`) are serialised per session_id and the
  consistent-hash partition assignment is stable
  """

  use GenServer

  @table :rockbox_session_route

  defmodule Route do
    @enforce_keys [:session_id, :vm_id, :partition]
    defstruct [:session_id, :vm_id, :partition, :workspace_id, :last_seen]
  end

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  @doc """
  Resolve a session to its VM. Returns `{:ok, route}` if known, `:miss` if
  not yet registered.
  """
  def lookup(session_id) do
    case :ets.lookup(@table, session_id) do
      [{_, route}] -> {:ok, route}
      [] -> :miss
    end
  end

  @doc "Create a new mapping for the given session."
  def register(session_id, vm_id, workspace_id),
    do: GenServer.call(__MODULE__, {:register, session_id, vm_id, workspace_id})

  @doc "Forget the mapping (called on session destroy)."
  def forget(session_id), do: GenServer.cast(__MODULE__, {:forget, session_id})

  @doc "Stable partition index for a session_id. Same hash used by the supervisor."
  def partition_for(session_id, partitions) when partitions > 0 do
    :erlang.phash2(session_id, partitions)
  end

  @impl true
  def init(_) do
    :ets.new(@table, [:named_table, :public, read_concurrency: true])
    {:ok, %{}}
  end

  @impl true
  def handle_call({:register, sid, vm_id, wid}, _from, state) do
    partitions = max(1, System.schedulers_online())

    route = %Route{
      session_id: sid,
      vm_id: vm_id,
      partition: partition_for(sid, partitions),
      workspace_id: wid,
      last_seen: System.system_time(:millisecond)
    }

    :ets.insert(@table, {sid, route})
    {:reply, {:ok, route}, state}
  end

  @impl true
  def handle_cast({:forget, sid}, state) do
    :ets.delete(@table, sid)
    {:noreply, state}
  end
end
