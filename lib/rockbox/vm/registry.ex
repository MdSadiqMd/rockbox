defmodule Rockbox.VM.Registry do
  @moduledoc "Registry for VM lookup by id. Used as a `:via` tuple in VM.Server"

  def child_spec(_) do
    Registry.child_spec(
      keys: :unique,
      name: __MODULE__,
      partitions: System.schedulers_online()
    )
  end

  def via(vm_id), do: {:via, Registry, {__MODULE__, vm_id}}

  def whereis(vm_id) do
    case Registry.lookup(__MODULE__, vm_id) do
      [{pid, _}] -> {:ok, pid}
      [] -> :error
    end
  end

  def list, do: Registry.select(__MODULE__, [{{:"$1", :"$2", :_}, [], [{{:"$1", :"$2"}}]}])
end
