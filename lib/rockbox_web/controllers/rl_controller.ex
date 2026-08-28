defmodule RockboxWeb.RLController do
  @moduledoc """
  Reinforcement-learning endpoints. Thin wrappers over `Pool.Manager` +
  `VM.Server`; each RL VM stays hot between `reset` and the final `done`
  step so state accumulates on the `/episode/` bind volume

  Contract (Gymnasium-compatible):

  - `POST /api/rl/episodes` — starts an episode. Settings must set
    `mode = rl_step`. Response carries `{episode_id, initial}` where
    `initial` is the reset tick `{observation, info, obs_meta?, ...}`.
    Deterministic seeding: pass `"determinism": {"seed": <u64>}` in settings
    and it reaches the env's `reset(seed=...)`.
  - `POST /api/rl/episodes/:episode_id/step` — sends base64 action bytes,
    replies with the tick `{observation, reward, done, terminated, truncated,
    info, obs_meta?}`. The VM is resolved server-side via EpisodeRegistry —
    callers never handle `vm_id`.
  - `POST /api/rl/episodes/:episode_id/steps` — EnvPool-style batched steps:
    `{"actions": ["<b64>", ...]}` runs N sequential steps through one engine
    round trip and returns every tick plus cumulative episode metrics
    (`steps`, `reward_sum`, `elapsed_ms`) in a single response. This is the
    hot path for rollout loops — ~N× fewer HTTP + orchestrator hops than
    single stepping.
  - `DELETE /api/rl/episodes/:episode_id` — tears the VM down

  Tick semantics: `terminated` marks a true environment end state;
  `truncated` marks a time-limit stop; `done = terminated || truncated`
  (back-compat). Env errors surface as `done=true` + `info["error"]`.
  """

  use Phoenix.Controller, formats: [:json]
  alias Rockbox.EpisodeRegistry
  alias Rockbox.EpisodeStore
  alias Rockbox.Pool.Manager, as: Pool
  alias Rockbox.QuotaTracker
  alias Rockbox.Settings.{Effective, Pipeline}
  alias Rockbox.{VM, AuditLog}

  def create(conn, %{"settings" => payload}) do
    ctx = caller_ctx(conn)

    # Force the mode; the caller might send `exec` by mistake.
    payload = Map.put(payload, "mode", "rl_step")

    with {:ok, %Effective{} = eff} <- Pipeline.run(payload, ctx),
         {:ok, vm_id} <- Pool.acquire(eff) do
      timeout = (eff.limits["wall_ms"] || 5_000) + 5_000

      case VM.Server.execute_and_wait(vm_id, eff, timeout) do
        {:ok, %{"status" => "success", "output" => output}} ->
          EpisodeRegistry.register(eff.request_id, vm_id, eff.workspace_id, eff.tier || :pro)
          pin_owner!(eff.request_id, eff.workspace_id)

          initial =
            case Jason.decode(output) do
              {:ok, %{} = j} -> j
              _ -> %{}
            end

          json(conn, %{
            episode_id: eff.request_id,
            vm_id: vm_id,
            initial: initial,
            seed: eff.determinism["seed"]
          })

        {:ok, %{"status" => status} = res} ->
          Pool.release(vm_id, eff)

          conn
          |> put_status(502)
          |> json(%{
            error: "rl_start_failed",
            status: status,
            errors: res["errors"],
            exit_code: res["exit_code"]
          })

        {:error, %{engine_died: info}} ->
          Pool.release(vm_id, eff)
          conn |> put_status(500) |> json(%{error: "engine_died", info: info})

        {:error, _reason} ->
          Pool.release(vm_id, eff)
          conn |> put_status(504) |> json(%{error: "timeout"})
      end
    else
      {:error, violations} when is_list(violations) ->
        conn |> put_status(422) |> json(%{error: "settings_invalid", violations: violations})

      {:error, :concurrency_exceeded} ->
        conn |> put_status(429) |> json(%{error: "concurrency_exceeded"})

      {:error, reason} ->
        conn |> put_status(500) |> json(%{error: inspect(reason)})
    end
  end

  def create(conn, _params) do
    conn |> put_status(400) |> json(%{error: "missing settings"})
  end

  def step(conn, %{"episode_id" => eid} = params) do
    ctx = caller_ctx(conn)
    action_param = params["action"] || params[:action]

    case owned_route!(conn, params, eid) do
      {:ok, _vm_id, _wid, _tier} ->
        with {:ok, action_bytes} <- decode_action(action_param),
             {:ok, msg, wid} <- step_with_resurrection(eid, params, action_bytes, ctx) do
          QuotaTracker.bump_steps(wid, 1)
          json(conn, tick_json(eid, msg))
        else
          {:error, :bad_action} ->
            conn |> put_status(400) |> json(%{error: "action must be base64-encoded bytes"})

          {:error, %{engine_died: info}} ->
            conn |> put_status(500) |> json(%{error: "engine_died", info: info})

          {:error, :forbidden} ->
            conn |> put_status(403) |> json(%{error: "forbidden"})

          {:error, :episode_not_found} ->
            conn |> put_status(404) |> json(%{error: "episode_not_found"})

          {:error, _} ->
            conn |> put_status(504) |> json(%{error: "timeout"})
        end

      {:error, :forbidden} ->
        conn |> put_status(403) |> json(%{error: "forbidden"})

      {:error, :episode_not_found} ->
        # 404 takes precedence over 400 for unknown episodes — matches the
        # existing test expectation and avoids leaking whether an action
        # would have been valid for a non-existent episode.
        case EpisodeStore.fetch_settings(eid) do
          :error ->
            conn |> put_status(404) |> json(%{error: "episode_not_found"})

          {:ok, _} ->
            if owns_durable_episode?(eid, ctx.workspace_id) do
              with {:ok, action_bytes} <- decode_action(action_param),
                   {:ok, msg, wid} <- step_with_resurrection(eid, params, action_bytes, ctx) do
                QuotaTracker.bump_steps(wid, 1)
                json(conn, tick_json(eid, msg))
              else
                {:error, :bad_action} ->
                  conn |> put_status(400) |> json(%{error: "action must be base64-encoded bytes"})

                {:error, :episode_not_found} ->
                  conn |> put_status(404) |> json(%{error: "episode_not_found"})

                _ ->
                  conn |> put_status(504) |> json(%{error: "timeout"})
              end
            else
              conn |> put_status(403) |> json(%{error: "forbidden"})
            end
        end
    end
  end

  defp step_with_resurrection(eid, params, action_bytes, ctx) do
    case resolve_route(params, eid) do
      {:ok, vm_id, wid, tier} ->
        case VM.Server.rl_step_wait(vm_id, eid, action_bytes) do
          {:ok, msg} ->
            {:ok, msg, wid}

          {:error, _} ->
            with {:ok, new_vm_id} <- resurrect_episode(eid, wid, tier),
                 {:ok, msg} <- VM.Server.rl_step_wait(new_vm_id, eid, action_bytes) do
              {:ok, msg, wid}
            else
              err -> err
            end
        end

      {:error, :episode_not_found} ->
        with {:ok, new_vm_id} <- resurrect_episode(eid, ctx.workspace_id, ctx.tier),
             {:ok, msg} <- VM.Server.rl_step_wait(new_vm_id, eid, action_bytes) do
          {:ok, msg, ctx.workspace_id}
        else
          _ -> {:error, :episode_not_found}
        end
    end
  end

  @doc """
  Batched steps: `{"actions": ["<b64>", ...]}` → `{"ticks": [...], "metrics":
  {steps, reward_sum, elapsed_ms}}`. One engine round trip for the whole
  batch; a mid-batch failure terminates remaining ticks with done=true error
  entries so clients can unwind without special-casing.
  """
  def steps(conn, %{"episode_id" => eid} = params) do
    ctx = caller_ctx(conn)
    actions_param = params["actions"] || params[:actions]

    case resolve_route(params, eid) do
      {:ok, _vm_id, _wid, _tier} ->
        with {:ok, actions} <- decode_actions(actions_param),
             {:ok, ticks, metrics, wid} <- steps_with_resurrection(eid, params, actions, ctx) do
          QuotaTracker.bump_steps(wid, length(ticks))

          json(conn, %{
            episode_id: eid,
            ticks: Enum.map(ticks, &tick_json(eid, &1)),
            metrics: metrics || %{}
          })
        else
          {:error, :bad_actions} ->
            conn
            |> put_status(400)
            |> json(%{error: "actions must be a list of base64-encoded byte strings"})

          {:error, :empty_batch} ->
            conn |> put_status(400) |> json(%{error: "actions must not be empty"})

          {:error, :episode_not_found} ->
            conn |> put_status(404) |> json(%{error: "episode_not_found"})

          {:error, %{engine_died: info}} ->
            conn |> put_status(500) |> json(%{error: "engine_died", info: info})

          {:error, _} ->
            conn |> put_status(504) |> json(%{error: "timeout"})
        end

      {:error, :episode_not_found} ->
        case EpisodeStore.fetch_settings(eid) do
          :error ->
            conn |> put_status(404) |> json(%{error: "episode_not_found"})

          {:ok, _} ->
            with {:ok, actions} <- decode_actions(actions_param),
                 {:ok, ticks, metrics, wid} <- steps_with_resurrection(eid, params, actions, ctx) do
              QuotaTracker.bump_steps(wid, length(ticks))

              json(conn, %{
                episode_id: eid,
                ticks: Enum.map(ticks, &tick_json(eid, &1)),
                metrics: metrics || %{}
              })
            else
              {:error, :bad_actions} ->
                conn
                |> put_status(400)
                |> json(%{error: "actions must be a list of base64-encoded byte strings"})

              {:error, :empty_batch} ->
                conn |> put_status(400) |> json(%{error: "actions must not be empty"})

              {:error, :episode_not_found} ->
                conn |> put_status(404) |> json(%{error: "episode_not_found"})

              {:error, _} ->
                conn |> put_status(504) |> json(%{error: "timeout"})
            end
        end
    end
  end

  defp steps_with_resurrection(eid, params, actions, ctx) do
    case resolve_route(params, eid) do
      {:ok, vm_id, wid, tier} ->
        case VM.Server.rl_steps_wait(vm_id, eid, actions) do
          {:ok, %{"ticks" => ticks} = msg} ->
            {:ok, ticks, msg["metrics"], wid}

          {:error, _} ->
            with {:ok, new_vm_id} <- resurrect_episode(eid, wid, tier),
                 {:ok, %{"ticks" => ticks} = msg} <-
                   VM.Server.rl_steps_wait(new_vm_id, eid, actions) do
              {:ok, ticks, msg["metrics"], wid}
            else
              err -> err
            end
        end

      {:error, :episode_not_found} ->
        with {:ok, new_vm_id} <- resurrect_episode(eid, ctx.workspace_id, ctx.tier),
             {:ok, %{"ticks" => ticks} = msg} <- VM.Server.rl_steps_wait(new_vm_id, eid, actions) do
          {:ok, ticks, msg["metrics"], ctx.workspace_id}
        else
          _ -> {:error, :episode_not_found}
        end
    end
  end

  @doc """
  Pause an episode: stop its worker VM but KEEP the durable episode volume
  (manifest + state snapshot). The next `step` transparently resurrects the
  worker from the checkpoint — same path as crash recovery. Envs that define
  save()/restore() get per-step snapshots, so pause is lossless for them;
  envs without save() restart from reset() on resume.
  """
  def pause(conn, %{"episode_id" => eid} = params) do
    case resolve_route(params, eid) do
      {:ok, vm_id, workspace_id, _tier} ->
        EpisodeRegistry.forget(eid)
        VM.Supervisor.stop_vm(vm_id, :rl_episode_paused)
        GenServer.cast(Rockbox.Pool.Manager, {:vm_dead, vm_id, workspace_id})

        _ =
          AuditLog.record(%{request_id: eid, workspace_id: workspace_id, status: "rl_paused"})

        json(conn, %{ok: true, episode_id: eid, paused: true})

      {:error, :episode_not_found} ->
        # No live route: either already paused or never existed. The volume
        # distinguishes them; a paused episode stays pausable idempotently.
        case EpisodeStore.fetch_settings(eid) do
          {:ok, _} -> json(conn, %{ok: true, episode_id: eid, paused: true, already_paused: true})
          :error -> conn |> put_status(404) |> json(%{error: "episode_not_found"})
        end
    end
  end

  def delete(conn, %{"episode_id" => eid} = params) do
    case resolve_route(params, eid) do
      {:ok, vm_id, workspace_id, _tier} ->
        EpisodeRegistry.forget(eid)
        VM.Supervisor.stop_vm(vm_id, :rl_episode_done)
        GenServer.cast(Rockbox.Pool.Manager, {:vm_dead, vm_id, workspace_id})
        EpisodeStore.remove_episode(eid)

        _ =
          AuditLog.record(%{request_id: eid, workspace_id: workspace_id, status: "rl_destroyed"})

        json(conn, %{ok: true, episode_id: eid})

      {:error, :episode_not_found} ->
        # Durable episodes (paused / VM-lost) are owner-checked before removal.
        if owns_durable_episode?(eid, conn.assigns.caller_workspace) do
          EpisodeStore.remove_episode(eid)
          json(conn, %{ok: true, episode_id: eid, already_gone: true})
        else
          conn |> put_status(404) |> json(%{error: "episode_not_found"})
        end
    end
  end

  defp resolve_route(params, eid) do
    case params["vm_id"] || params[:vm_id] do
      vm_id when is_binary(vm_id) ->
        {:ok, vm_id, nil, :pro}

      _ ->
        case EpisodeRegistry.lookup_route(eid) do
          {:ok, vm_id, wid, tier} -> {:ok, vm_id, wid, tier}
          :miss -> {:error, :episode_not_found}
        end
    end
  end

  # Resolve and enforce that a live episode belongs to the caller. Episodes
  # whose route predates ownership metadata (wid = nil) pass through.
  defp owned_route!(conn, params, eid) do
    case resolve_route(params, eid) do
      {:ok, vm_id, nil, tier} ->
        {:ok, vm_id, nil, tier}

      {:ok, vm_id, wid, tier} ->
        if wid == conn.assigns.caller_workspace,
          do: {:ok, vm_id, wid, tier},
          else: {:error, :forbidden}

      err ->
        err
    end
  end

  # Pin the owning workspace into the frozen manifest so durable episodes
  # (paused / VM-lost) can only be resumed or deleted by their owner.
  defp pin_owner!(episode_id, workspace_id) do
    path = Path.join([EpisodeStore.episodes_root(), episode_id, "manifest.json"])

    with {:ok, body} <- File.read(path),
         {:ok, map} <- Jason.decode(body) do
      File.write(path, Jason.encode!(Map.put(map, "workspace_id", workspace_id)))
    end

    :ok
  end

  # Durable episodes survive VM loss; the manifest pins the owner. Returns
  # :ok when the caller owns it (or ownership can't be determined for
  # legacy manifests created before pinning — those stay registry-only).
  defp owns_durable_episode?(eid, caller_workspace) do
    case EpisodeStore.fetch_settings(eid) do
      {:ok, %{"workspace_id" => wid}} -> wid == caller_workspace
      _ -> false
    end
  end

  defp resurrect_episode(eid, workspace_id, tier) do
    require Logger
    Logger.info("resurrecting episode #{eid} for workspace #{workspace_id}")

    case EpisodeRegistry.claim_restore(eid) do
      :in_progress ->
        Logger.info("resurrect claim in_progress for #{eid}, waiting")
        wait_for_resurrection(eid, 20_000)

      :ok ->
        try do
          case EpisodeStore.fetch_settings(eid) do
            {:ok, settings} ->
              # workspace_id is orchestrator ownership metadata pinned into
              # the manifest by pin_owner!/1 — not a sandbox setting. Strip
              # it before re-running the (strict) settings pipeline.
              settings = Map.delete(settings, "workspace_id")
              ctx = %{workspace_id: workspace_id, tier: tier || :pro, user_id: nil}

              with {:ok, %Effective{} = eff} <- Pipeline.run(settings, ctx) do
                Logger.info("resurrect pipeline ok for #{eid}, acquiring VM")

                case Pool.acquire(eff) do
                  {:ok, vm_id} ->
                    timeout = (eff.limits["wall_ms"] || 5_000) + 5_000
                    Logger.info("resurrect acquired #{vm_id} for #{eid}, executing")

                    case VM.Server.execute_and_wait(vm_id, eff, timeout) do
                      {:ok, %{"status" => "success"}} ->
                        Logger.info("resurrect success for #{eid} on #{vm_id}")
                        EpisodeRegistry.register(eid, vm_id, workspace_id, tier || :pro)
                        {:ok, vm_id}

                      {:ok, %{"status" => status} = res} ->
                        Logger.warning(
                          "resurrect failed for #{eid}: #{status} #{inspect(res["errors"])} "
                        )

                        Pool.release(vm_id, eff)
                        {:error, {:resurrect_failed, status, res["errors"]}}

                      {:error, reason} ->
                        Logger.warning(
                          "resurrect execute_and_wait failed for #{eid}: #{inspect(reason)}"
                        )

                        Pool.release(vm_id, eff)
                        {:error, reason}
                    end

                  {:error, reason} ->
                    Logger.warning("resurrect Pool.acquire failed for #{eid}: #{inspect(reason)}")
                    {:error, reason}
                end
              else
                {:error, reason} ->
                  Logger.warning("resurrect Pipeline.run failed for #{eid}: #{inspect(reason)}")
                  {:error, reason}
              end

            :error ->
              Logger.warning("resurrect no manifest for #{eid}")
              {:error, :no_manifest}
          end
        after
          EpisodeRegistry.restore_done(eid)
        end
    end
  end

  defp wait_for_resurrection(eid, timeout_ms) do
    deadline = System.monotonic_time(:millisecond) + timeout_ms
    wait_loop(eid, deadline)
  end

  defp wait_loop(eid, deadline) do
    if System.monotonic_time(:millisecond) > deadline do
      {:error, :timeout}
    else
      case EpisodeRegistry.lookup_route(eid) do
        {:ok, vm_id, _wid, _tier} ->
          {:ok, vm_id}

        :miss ->
          Process.sleep(100)
          wait_loop(eid, deadline)
      end
    end
  end

  # Normalised Gymnasium-style tick shape shared by single and batched paths.
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
