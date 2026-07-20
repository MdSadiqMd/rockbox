defmodule Rockbox.VM.Supervisor do
  @moduledoc """
  PartitionSupervisor-wrapped DynamicSupervisor that owns one VM GenServer
  per active engine process

  Stable shard key: we partition on `:erlang.phash2(vm_id)` so
  session-mode routes are deterministic even if `System.schedulers_online()`
  changes across deploys. The partition count is fixed at boot time
  """

  use Supervisor

  @partitions Application.compile_env(:rockbox, :vm_partitions, 16)

  def start_link(_), do: Supervisor.start_link(__MODULE__, [], name: __MODULE__)

  @impl true
  def init(_) do
    children = [
      {PartitionSupervisor,
       child_spec: DynamicSupervisor,
       name: __MODULE__.Pool,
       partitions: @partitions,
       with_arguments: fn args, _idx -> args end}
    ]

    Supervisor.init(children, strategy: :one_for_one)
  end

  @doc "Start a new VM under the right partition"
  def start_vm(opts) when is_map(opts) do
    vm_id = Map.fetch!(opts, :vm_id)
    partition = :erlang.phash2(vm_id, @partitions)
    spec = {Rockbox.VM.Server, opts}
    DynamicSupervisor.start_child({:via, PartitionSupervisor, {__MODULE__.Pool, partition}}, spec)
  end

  @doc "Stop a VM cleanly (sends `shutdown` over the Port, then terminates)"
  def stop_vm(vm_id, reason \\ :normal) do
    case Rockbox.VM.Registry.whereis(vm_id) do
      {:ok, pid} ->
        GenServer.cast(pid, :graceful_shutdown)

        DynamicSupervisor.terminate_child(
          {:via, PartitionSupervisor, {__MODULE__.Pool, :erlang.phash2(vm_id, @partitions)}},
          pid
        )

      :error ->
        {:error, :not_found}
    end
    |> tap(fn _ ->
      Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{vm_id}", {:vm_stopped, reason})
    end)
  end
end
