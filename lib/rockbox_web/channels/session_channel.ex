defmodule RockboxWeb.SessionChannel do
  @moduledoc "WebSocket for session-mode interactive REPL"

  use Phoenix.Channel
  alias Rockbox.{SessionRouter, VM, Wire}

  @impl true
  def join("session:" <> sid, _params, socket) do
    case SessionRouter.lookup(sid) do
      {:ok, route} ->
        Phoenix.PubSub.subscribe(Rockbox.PubSub, "vm:#{route.vm_id}")
        {:ok, socket |> assign(:session_id, sid) |> assign(:vm_id, route.vm_id)}

      :miss ->
        {:error, %{reason: "session_not_found"}}
    end
  end

  @impl true
  def handle_in("exec_cell", %{"code" => code} = params, socket) do
    cell =
      Wire.exec_cell(
        params["id"] || req_id(),
        socket.assigns.session_id,
        code,
        files: params["files"] || []
      )

    VM.Server.exec_cell(socket.assigns.vm_id, cell)
    {:noreply, socket}
  end

  @impl true
  def handle_info({:cell_result, payload}, socket) do
    push(socket, "session:cell_result", payload)
    {:noreply, socket}
  end

  def handle_info({:stdout, bytes}, socket) do
    push(socket, "session:output", %{stream: "stdout", data: Base.encode64(bytes)})
    {:noreply, socket}
  end

  def handle_info({:stderr, bytes}, socket) do
    push(socket, "session:output", %{stream: "stderr", data: Base.encode64(bytes)})
    {:noreply, socket}
  end

  def handle_info(_other, socket), do: {:noreply, socket}

  defp req_id, do: "req_" <> (:crypto.strong_rand_bytes(8) |> Base.encode16(case: :lower))
end
