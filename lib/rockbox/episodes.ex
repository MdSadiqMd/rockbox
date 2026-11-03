defmodule Rockbox.Episodes do
  @moduledoc """
  Episode lifecycle shared by the RL REST controller and the OpenEnv façade:
  boot a worker for a settings payload, step it, and release it back so the
  engine process stays pooled. Route resolution goes through `EpisodeRegistry`;
  callers never see `vm_id`.
  """

  alias Rockbox.{AuditLog, EpisodeRegistry, EpisodeStore, VM}
  alias Rockbox.Pool.Manager, as: Pool
  alias Rockbox.Settings.{Effective, Pipeline}

  @doc """
  Boot an RL episode from a settings payload. Returns
  `{:ok, %{episode_id, vm_id, initial, seed}}` where `initial` is the reset
  tick (observation base64-encoded), or `{:error, reason}` with reason one of
  `violations :: list`, `:concurrency_exceeded`, `{:rl_start_failed, status,
  result}`, `{:engine_died, info}`, `:timeout`.
  """
  def start(payload, ctx) do
    # Ids carry this node's cluster tag so any node can route them.
    payload =
      payload
      |> Map.put("mode", "rl_step")
      |> Map.put_new("request_id", Rockbox.Cluster.mint_episode_id())

    with {:ok, %Effective{} = eff} <- Pipeline.run(payload, ctx),
         {:ok, vm_id} <- Pool.acquire(eff) do
      timeout = (eff.limits["wall_ms"] || 5_000) + 5_000

      case VM.Server.execute_and_wait(vm_id, eff, timeout) do
        {:ok, %{"status" => "success", "output" => output}} ->
          EpisodeRegistry.register(eff.request_id, vm_id, eff.workspace_id, eff.tier || :pro, eff)
          # Ownership pinning only matters for durable resurrection; keep the
          # manifest rewrite (~0.5 ms of disk I/O) off the create path.
          Task.start(fn -> pin_owner(eff.request_id, eff.workspace_id) end)
          :telemetry.execute([:rockbox, :rl, :episode], %{count: 1}, %{event: :start})

          initial =
            case Jason.decode(output) do
              {:ok, %{} = j} -> j
              _ -> %{}
            end

          {:ok,
           %{
             episode_id: eff.request_id,
             vm_id: vm_id,
             initial: initial,
             seed: eff.determinism["seed"]
           }}

        {:ok, %{"status" => status} = res} ->
          Pool.release(vm_id, eff)
          {:error, {:rl_start_failed, status, res}}

        {:error, %{engine_died: info}} ->
          Pool.release(vm_id, eff)
          {:error, {:engine_died, info}}

        {:error, _reason} ->
          Pool.release(vm_id, eff)
          {:error, :timeout}
      end
    end
  end

  @doc """
  Run `actions` (raw bytes, in order) on a live episode in one engine round
  trip. Returns `{:ok, ticks, metrics}`; an empty list just reports metrics.
  """
  def steps(episode_id, actions) when is_list(actions) do
    case EpisodeRegistry.lookup_route(episode_id) do
      {:ok, vm_id, _wid, _tier} ->
        case VM.Server.rl_steps_wait(vm_id, episode_id, actions) do
          {:ok, %{"ticks" => ticks} = msg} -> {:ok, ticks, msg["metrics"] || %{}}
          {:error, _} = err -> err
        end

      :miss ->
        {:error, :episode_not_found}
    end
  end

  @doc "Workspace that owns a live episode, or `:miss`."
  def owner(episode_id) do
    case EpisodeRegistry.lookup_route(episode_id) do
      {:ok, _vm_id, wid, _tier} -> {:ok, wid}
      :miss -> :miss
    end
  end

  @doc """
  Tear down a live episode and its durable volume. Idempotent: an unknown id
  is a no-op.
  """
  def stop(episode_id, workspace_id) do
    case EpisodeRegistry.lookup_route(episode_id) do
      {:ok, vm_id, _wid, _tier} ->
        release(vm_id, episode_id, workspace_id)
        EpisodeStore.remove_episode(episode_id)

        _ =
          AuditLog.record(%{
            request_id: episode_id,
            workspace_id: workspace_id,
            status: "rl_destroyed"
          })

        :telemetry.execute([:rockbox, :rl, :episode], %{count: 1}, %{event: :stop})
        :ok

      :miss ->
        EpisodeStore.remove_episode(episode_id)
        :ok
    end
  end

  @doc """
  Stop an episode's worker but keep the engine: the worker is killed and the
  VM handed back to its pool bucket, so the next episode skips engine boot.
  Falls back to stopping the VM when the route predates settings capture or
  the engine does not answer.
  """
  def release(vm_id, episode_id, workspace_id) do
    settings = EpisodeRegistry.lookup_settings(episode_id)
    EpisodeRegistry.forget(episode_id)

    with %Effective{} = eff <- settings,
         {:ok, %{"info" => %{"closed" => _}}} <- VM.Server.rl_close_wait(vm_id, episode_id) do
      Pool.release(vm_id, eff)
    else
      _ ->
        VM.Supervisor.stop_vm(vm_id, :rl_episode_done)
        GenServer.cast(Rockbox.Pool.Manager, {:vm_dead, vm_id, workspace_id})
    end

    :ok
  end

  # Pin the owning workspace into the frozen manifest so durable episodes
  # (paused / VM-lost) can only be resumed or deleted by their owner.
  defp pin_owner(episode_id, workspace_id) do
    path = Path.join([EpisodeStore.episodes_root(), episode_id, "manifest.json"])

    with {:ok, body} <- File.read(path),
         {:ok, map} <- Jason.decode(body) do
      File.write(path, Jason.encode!(Map.put(map, "workspace_id", workspace_id)))
    end

    :ok
  end
end
