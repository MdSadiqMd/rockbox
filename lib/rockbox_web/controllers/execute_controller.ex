defmodule RockboxWeb.ExecuteController do
  @moduledoc """
  `POST /api/execute` - one-shot run. Synchronous from the caller's POV:
  resolves settings, acquires a VM, awaits the `result` event, and replies
  Streaming clients use the WebSocket channel instead
  """

  use Phoenix.Controller, formats: [:json]
  alias Rockbox.Pool.Manager, as: Pool
  alias Rockbox.Settings.{Effective, Pipeline}
  alias Rockbox.{VM, AuditLog}

  def create(conn, %{"settings" => settings_payload}) do
    ctx = %{
      workspace_id: conn.assigns.caller_workspace,
      tier: conn.assigns.caller_tier,
      user_id: conn.assigns.caller[:user_id]
    }

    audit_sink = fn %Effective{} = eff ->
      AuditLog.record(%{
        request_id: eff.request_id,
        workspace_id: eff.workspace_id,
        mode: eff.mode,
        language: eff.language,
        runtime: eff.runtime,
        settings_requested: settings_payload,
        settings_effective: scrub_secrets(eff),
        clamped: eff.clamped,
        status: "accepted"
      })
    end

    case Pipeline.run(settings_payload, ctx, audit_sink: audit_sink) do
      {:ok, %Effective{} = effective} ->
        run_and_reply(conn, effective)

      {:error, violations} ->
        conn
        |> put_status(422)
        |> json(%{error: "settings_invalid", violations: violations})
    end
  end

  def create(conn, _params) do
    conn
    |> put_status(400)
    |> json(%{error: "missing `settings` object"})
  end

  defp run_and_reply(conn, %Effective{} = eff) do
    timeout = (eff.limits["wall_ms"] || 5_000) + 5_000

    case Pool.acquire(eff) do
      {:ok, vm_id} ->
        case VM.Server.execute_and_wait(vm_id, eff, timeout) do
          {:ok, result} ->
            Pool.release(vm_id, eff)
            json(conn, build_response(eff, vm_id, result))

          {:error, %{engine_died: status}} ->
            Pool.release(vm_id, eff)
            conn |> put_status(500) |> json(%{error: "engine_died", info: status})

          {:error, :timeout} ->
            Pool.release(vm_id, eff)
            conn |> put_status(504) |> json(%{error: "timeout"})

          {:error, reason} ->
            Pool.release(vm_id, eff)
            conn |> put_status(502) |> json(%{error: "execute_failed", reason: inspect(reason)})
        end

      {:error, :concurrency_exceeded} ->
        conn |> put_status(429) |> json(%{error: "concurrency_exceeded"})

      {:error, reason} ->
        conn |> put_status(500) |> json(%{error: "acquire_failed", reason: inspect(reason)})
    end
  end

  defp build_response(%Effective{} = eff, vm_id, result) do
    %{
      status: result["status"],
      request_id: eff.request_id,
      vm_id: vm_id,
      session_id: eff.session_id,
      exit_code: result["exit_code"],
      output: result["output"],
      errors: result["errors"],
      exec_time_ms: result["exec_time_ms"],
      memory_peak_mb: result["memory_peak_mb"],
      output_truncated: result["output_truncated"],
      settings_effective: scrub_secrets(eff),
      clamped: eff.clamped,
      warnings: []
    }
  end

  defp scrub_secrets(%Effective{} = eff) do
    eff
    |> Map.from_struct()
    |> Map.delete(:resolved_secrets)
  end
end
