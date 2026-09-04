defmodule RockboxWeb.EpisodeChannel do
  @moduledoc """
  WebSocket for RL-mode low-latency stepping (SOTA streaming path).

  Why a dedicated channel: REST `POST /api/rl/episodes/:id/step` pays
  ~0.3-0.5ms per step in HTTP framing (headers, JSON, Plug parsers) that
  EnvPool and Brax avoid by staying in-process. A single WebSocket
  holds one TCP connection, frames are raw msgpack/binary, and steps
  are pipelined without HTTP. For batched steps the same socket carries
  64 actions in one frame, mirroring EnvPool's vectorized API.

  Protocol:
  - join "episode:<episode_id>" — binds to that episode's VM, subscribes
    to "vm:<vm_id>" for stdout/stderr forwarding
  - handle_in "step" %{"action" => base64} -> push "episode:tick" with
    tick_json or raw binary if client negotiated msgpack
  - handle_in "steps" %{"actions" => [base64]} -> push "episode:ticks"
  - handle_in "step_binary" %{"action" => <<bytes>>} -> zero-copy path,
    no base64 encode/decode

  This is the SOTA RL infra streaming layer: persistent connection,
  binary transport, per-episode VM affinity, single-hop direct reply
  (no PubSub broadcast when waiter exists, see VM.Server optimization).
  """

  use Phoenix.Channel
  alias Rockbox.{EpisodeRegistry, VM}

  @impl true
  def join("episode:" <> eid, _params, socket) do
    case EpisodeRegistry.lookup_route(eid) do
      {:ok, vm_id, _wid, _tier} ->
        Phoenix.PubSub.subscribe(Rockbox.PubSub, "vm:#{vm_id}")

        {:ok,
         socket
         |> assign(:episode_id, eid)
         |> assign(:vm_id, vm_id)}

      :miss ->
        {:error, %{reason: "episode_not_found"}}
    end
  end

  @impl true
  def handle_in("step", %{"action" => action_param} = _params, socket) do
    eid = socket.assigns.episode_id
    vm_id = socket.assigns.vm_id

    with {:ok, action_bytes} <- decode_action(action_param),
         {:ok, msg} <- VM.Server.rl_step_wait(vm_id, eid, action_bytes) do
      push(socket, "episode:tick", tick_json(eid, msg))
    else
      {:error, :bad_action} -> push(socket, "error", %{reason: "bad_action"})
      {:error, _} -> push(socket, "error", %{reason: "step_failed"})
    end

    {:noreply, socket}
  end

  def handle_in("step_binary", %{"action" => action_bytes}, socket)
      when is_binary(action_bytes) do
    eid = socket.assigns.episode_id
    vm_id = socket.assigns.vm_id

    case VM.Server.rl_step_wait(vm_id, eid, action_bytes) do
      {:ok, msg} -> push(socket, "episode:tick", tick_json(eid, msg))
      {:error, _} -> push(socket, "error", %{reason: "step_failed"})
    end

    {:noreply, socket}
  end

  def handle_in("steps", %{"actions" => actions_param}, socket) do
    eid = socket.assigns.episode_id
    vm_id = socket.assigns.vm_id

    with {:ok, actions} <- decode_actions(actions_param),
         {:ok, %{"ticks" => ticks} = msg} <- VM.Server.rl_steps_wait(vm_id, eid, actions) do
      push(socket, "episode:ticks", %{
        episode_id: eid,
        ticks: Enum.map(ticks, &tick_json(eid, &1)),
        metrics: msg["metrics"] || %{}
      })
    else
      {:error, _} -> push(socket, "error", %{reason: "steps_failed"})
    end

    {:noreply, socket}
  end

  def handle_in(_event, _params, socket), do: {:noreply, socket}

  @impl true
  def handle_info({:stdout, bytes}, socket) do
    push(socket, "episode:output", %{stream: "stdout", data: Base.encode64(bytes)})
    {:noreply, socket}
  end

  def handle_info({:stderr, bytes}, socket) do
    push(socket, "episode:output", %{stream: "stderr", data: Base.encode64(bytes)})
    {:noreply, socket}
  end

  def handle_info(_other, socket), do: {:noreply, socket}

  defp tick_json(episode_id, msg) do
    %{
      episode_id: episode_id,
      request_id: msg["request_id"],
      observation: maybe_b64_encode(msg["observation"]),
      reward: msg["reward"],
      done: msg["done"],
      terminated: msg["terminated"] || false,
      truncated: msg["truncated"] || false,
      info: msg["info"] || %{},
      obs_meta: msg["obs_meta"]
    }
  end

  defp decode_action(nil), do: {:ok, <<>>}
  defp decode_action(""), do: {:ok, <<>>}

  defp decode_action(bin) when is_binary(bin) do
    case Base.decode64(bin) do
      {:ok, bytes} -> {:ok, bytes}
      :error -> {:error, :bad_action}
    end
  end

  defp decode_action(_), do: {:error, :bad_action}

  defp decode_actions(actions) when is_list(actions) do
    if actions == [] do
      {:error, :empty_batch}
    else
      Enum.reduce_while(actions, {:ok, []}, fn a, {:ok, acc} ->
        case decode_action(a) do
          {:ok, bytes} -> {:cont, {:ok, [bytes | acc]}}
          {:error, _} = err -> {:halt, err}
        end
      end)
      |> case do
        {:ok, list} -> {:ok, Enum.reverse(list)}
        err -> err
      end
    end
  end

  defp decode_actions(_), do: {:error, :bad_actions}

  defp maybe_b64_encode(nil), do: nil
  defp maybe_b64_encode(bin) when is_binary(bin), do: Base.encode64(bin)
  defp maybe_b64_encode(list) when is_list(list), do: Base.encode64(:binary.list_to_bin(list))
  defp maybe_b64_encode(other), do: other
end
