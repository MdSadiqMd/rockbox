defmodule RockboxWeb.RLController do
  @moduledoc """
  Reinforcement-learning endpoints. Thin wrappers over `Pool.Manager` +
  `VM.Server`; each RL VM stays hot between `reset` and the final `done`
  step so state accumulates on the `/episode/` bind volume

  Contract:

  - `POST /api/rl/episodes` — starts an episode. Settings must set
    `mode = rl_step`. Response carries `{episode_id, vm_id, initial}`
    where `initial` is the JSON tick emitted by the env's `reset()`
  - `POST /api/rl/episodes/:episode_id/step` — sends action bytes,
    replies with the tick JSON (observation, reward, done, info)
  - `DELETE /api/rl/episodes/:episode_id` — tears the VM down
  """

  use Phoenix.Controller, formats: [:json]
  alias Rockbox.Pool.Manager, as: Pool
  alias Rockbox.Settings.{Effective, Pipeline}
  alias Rockbox.{VM, AuditLog}

  def create(conn, %{"settings" => payload}) do
    ctx = caller_ctx(conn)

    # Force the mode; the caller might send `exec` by mistake.
    payload = Map.put(payload, "mode", "rl_step")

    with {:ok, %Effective{} = eff} <- Pipeline.run(payload, ctx),
         {:ok, vm_id} <- Pool.acquire(eff) do
      Phoenix.PubSub.subscribe(Rockbox.PubSub, "vm:#{vm_id}")

      case VM.Server.execute(vm_id, eff) do
        :ok ->
          wait_for_start(conn, vm_id, eff)

        {:error, reason} ->
          Pool.release(vm_id, eff)
          conn |> put_status(502) |> json(%{error: "start_failed", reason: inspect(reason)})
      end
    else
      {:error, violations} when is_list(violations) ->
        conn |> put_status(422) |> json(%{error: "settings_invalid", violations: violations})

      {:error, reason} ->
        conn |> put_status(500) |> json(%{error: inspect(reason)})
    end
  end

  def create(conn, _params) do
    conn |> put_status(400) |> json(%{error: "missing settings"})
  end

  def step(conn, %{"episode_id" => eid} = params) do
    vm_id = params["vm_id"] || conn.query_params["vm_id"]

    with true <- is_binary(vm_id) or {:error, :vm_id_required},
         {:ok, action_bytes} <- decode_action(params["action"]) do
      Phoenix.PubSub.subscribe(Rockbox.PubSub, "vm:#{vm_id}")
      :ok = VM.Server.rl_step(vm_id, eid, action_bytes)
      wait_for_step(conn, eid, action_bytes)
    else
      {:error, :vm_id_required} ->
        conn |> put_status(400) |> json(%{error: "vm_id required"})

      {:error, :bad_action} ->
        conn |> put_status(400) |> json(%{error: "action must be base64-encoded bytes"})

      false ->
        conn |> put_status(400) |> json(%{error: "vm_id required"})
    end
  end

  def delete(conn, %{"episode_id" => eid} = params) do
    vm_id = params["vm_id"] || conn.query_params["vm_id"]

    cond do
      not is_binary(vm_id) ->
        conn |> put_status(400) |> json(%{error: "vm_id required"})

      true ->
        VM.Supervisor.stop_vm(vm_id, :rl_episode_done)
        _ = AuditLog.record(%{request_id: eid, workspace_id: nil, status: "rl_destroyed"})
        json(conn, %{ok: true, episode_id: eid, vm_id: vm_id})
    end
  end

  defp wait_for_start(conn, vm_id, %Effective{} = eff) do
    timeout = (eff.limits["wall_ms"] || 5_000) + 5_000

    receive do
      {:result, %{"status" => "success", "output" => output}} ->
        initial =
          case Jason.decode(output) do
            {:ok, %{} = j} -> j
            _ -> %{}
          end

        json(conn, %{
          episode_id: eff.request_id,
          vm_id: vm_id,
          initial: initial
        })

      {:result, %{"status" => status} = res} ->
        Pool.release(vm_id, eff)

        conn
        |> put_status(502)
        |> json(%{
          error: "rl_start_failed",
          status: status,
          errors: res["errors"],
          exit_code: res["exit_code"]
        })

      {:engine_died, info} ->
        Pool.release(vm_id, eff)
        conn |> put_status(500) |> json(%{error: "engine_died", info: info})
    after
      timeout ->
        Pool.release(vm_id, eff)
        conn |> put_status(504) |> json(%{error: "timeout"})
    end
  end

  defp wait_for_step(conn, episode_id, _action) do
    receive do
      {:rl_step, %{"episode_id" => ^episode_id} = msg} ->
        json(conn, %{
          episode_id: episode_id,
          observation: msg["observation"] |> maybe_b64_encode(),
          reward: msg["reward"],
          done: msg["done"],
          info: msg["info"] || %{}
        })

      {:engine_died, info} ->
        conn |> put_status(500) |> json(%{error: "engine_died", info: info})
    after
      15_000 ->
        conn |> put_status(504) |> json(%{error: "timeout"})
    end
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

  # Msgpax decodes binary as either binary or list-of-ints depending on the
  # type marker; observation always comes back as binary, but stringifying it
  # for JSON transit would be lossy. Emit base64 so clients decode it back
  # deterministically.
  defp maybe_b64_encode(nil), do: nil
  defp maybe_b64_encode(bin) when is_binary(bin), do: Base.encode64(bin)
  defp maybe_b64_encode(list) when is_list(list), do: Base.encode64(:binary.list_to_bin(list))
  defp maybe_b64_encode(other), do: other

  defp caller_ctx(conn),
    do: %{
      workspace_id: conn.assigns.caller_workspace,
      tier: conn.assigns.caller_tier,
      user_id: conn.assigns.caller[:user_id]
    }
end
