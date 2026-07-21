defmodule RockboxWeb.VMChannel do
  @moduledoc "WebSocket relay for `vm:<vm_id>` events from PubSub to the client"

  use Phoenix.Channel
  alias Rockbox.VM

  @impl true
  def join("vm:" <> vm_id, _params, socket) do
    case VM.Registry.whereis(vm_id) do
      {:ok, _} ->
        Phoenix.PubSub.subscribe(Rockbox.PubSub, "vm:#{vm_id}")
        {:ok, assign(socket, :vm_id, vm_id)}

      :error ->
        {:error, %{reason: "vm_not_found"}}
    end
  end

  @impl true
  def handle_in("stdin", %{"data" => data}, socket) when is_binary(data) do
    VM.Server.stdin(socket.assigns.vm_id, data)
    {:noreply, socket}
  end

  def handle_in("interrupt", %{"id" => id}, socket) do
    VM.Server.interrupt(socket.assigns.vm_id, id)
    {:noreply, socket}
  end

  @impl true
  def handle_info({:stdout, bytes}, socket) do
    push(socket, "vm:output", %{stream: "stdout", data: Base.encode64(bytes)})
    {:noreply, socket}
  end

  def handle_info({:stderr, bytes}, socket) do
    push(socket, "vm:output", %{stream: "stderr", data: Base.encode64(bytes)})
    {:noreply, socket}
  end

  def handle_info({:result, payload}, socket) do
    push(socket, "vm:exited", payload)
    {:noreply, socket}
  end

  def handle_info({:engine_died, info}, socket) do
    push(socket, "vm:engine_died", %{info: info})
    {:stop, :normal, socket}
  end

  def handle_info(_other, socket), do: {:noreply, socket}
end
